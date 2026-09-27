//! The bwrap filesystem-restriction arg builder — the most intricate,
//! security-critical function of the package. Ported 1:1 from
//! `@anthropic-ai/sandbox-runtime@0.0.54`.
//!
//! Reference of truth (source-line cites throughout):
//! `docs/superpowers/references/sandbox-runtime-0.0.54/dist/sandbox/linux-sandbox-utils.js`.
//!
//! Contents:
//! - the three pure FS path-walk helpers ([`find_symlink_in_path`],
//!   [`has_file_ancestor`], [`find_first_non_existent_component`]) — `linux-sandbox-utils.js:21-100`.
//! - [`linux_get_mandatory_deny_paths`] — `linux-sandbox-utils.js:102-201`; shells `rg`.
//! - [`generate_filesystem_args`] — `linux-sandbox-utils.js:527-772`; the orchestrator.
//!
//! # Divergence from the TS (documented, intentional)
//!
//! The TS uses a module-global `bwrapMountPoints: Set<string>` plus
//! `registerExitCleanupHandler()` (`linux-sandbox-utils.js:202-284`) to track and
//! later delete the host mount-point files bwrap creates for non-existent deny
//! paths. This port has **no global state**: [`generate_filesystem_args`]
//! RETURNS `(args, mount_points)` so the caller (P4-2c) owns cleanup. This is
//! cleaner and testable; the security behavior (which mount points are created)
//! is identical.
//!
//! All path splitting uses `/` as the separator, matching `path.sep` on the
//! Linux target these functions are written for (bwrap is Linux-only).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::path_utils::{
    get_dangerous_directories, is_symlink_outside_boundary, normalize_case_for_comparison,
    normalize_path_for_sandbox, DANGEROUS_FILES,
};

/// Default max depth for searching dangerous files.
///
/// Ported from `linux-sandbox-utils.js:13` (`DEFAULT_MANDATORY_DENY_SEARCH_DEPTH`).
pub const DEFAULT_MANDATORY_DENY_SEARCH_DEPTH: usize = 3;

/// Read-restriction config (`denyOnly` pattern).
///
/// EMPTY `deny_only` = allow ALL reads (no read restrictions). Mirrors the TS
/// `ReadConfig` from `sandbox-schemas.d.ts`; serde camelCase matches the TS
/// `denyOnly`/`allowWithinDeny` field names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadConfig {
    /// Paths to deny read access to. Empty = allow all reads.
    #[serde(default)]
    pub deny_only: Vec<String>,
    /// Paths re-allowed for reading even inside a denied region.
    #[serde(default)]
    pub allow_within_deny: Vec<String>,
}

/// Write-restriction config (`allowOnly` pattern).
///
/// EMPTY `allow_only` = deny ALL writes (read-only root, nothing re-bound
/// writable). Mirrors the TS `WriteConfig` from `sandbox-schemas.d.ts`; serde
/// camelCase matches the TS `allowOnly`/`denyWithinAllow` field names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteConfig {
    /// Paths to allow writes to. Empty = deny all writes.
    #[serde(default)]
    pub allow_only: Vec<String>,
    /// Paths to deny writes to even inside an allowed region.
    #[serde(default)]
    pub deny_within_allow: Vec<String>,
}

/// Find if any component of the path is a symlink within the allowed write
/// paths. Returns the symlink path if found, or `None` if no symlinks.
///
/// Used to detect and block symlink replacement attacks where an attacker could
/// delete a symlink and create a real directory with malicious content.
///
/// Ported from `linux-sandbox-utils.js:21-44` (`findSymlinkInPath`).
#[must_use]
pub fn find_symlink_in_path(target_path: &str, allowed_write_paths: &[String]) -> Option<String> {
    let mut current_path = String::new();
    for part in target_path.split('/') {
        if part.is_empty() {
            continue; // Skip empty parts (leading /)
        }
        let next_path = format!("{current_path}/{part}");
        match std::fs::symlink_metadata(&next_path) {
            Ok(stats) => {
                if stats.file_type().is_symlink() {
                    // Check if this symlink is within an allowed write path.
                    let is_within_allowed_path = allowed_write_paths.iter().any(|allowed_path| {
                        next_path.starts_with(&format!("{allowed_path}/"))
                            || next_path == *allowed_path
                    });
                    if is_within_allowed_path {
                        return Some(next_path);
                    }
                }
            }
            Err(_) => {
                // Path doesn't exist - no symlink issue here.
                break;
            }
        }
        current_path = next_path;
    }
    None
}

/// Check if any existing component in the path is a file (not a directory).
/// If so, the target path can never be created because you can't mkdir under a
/// file.
///
/// This handles the git worktree case: `.git` is a file, so `.git/hooks` can
/// never exist and there's nothing to deny.
///
/// Ported from `linux-sandbox-utils.js:53-81` (`hasFileAncestor`).
#[must_use]
pub fn has_file_ancestor(target_path: &str) -> bool {
    let mut current_path = String::new();
    for part in target_path.split('/') {
        if part.is_empty() {
            continue; // Skip empty parts (leading /)
        }
        let next_path = format!("{current_path}/{part}");
        // TS: fs.statSync(nextPath) then `stat.isFile() || stat.isSymbolicLink()`.
        // statSync FOLLOWS symlinks, so for a live symlink it reports the
        // target's type — isSymbolicLink() is therefore never true here (a
        // dangling symlink makes statSync throw -> the catch breaks). So the
        // effective behavior is exactly: follow, file -> true, error -> break.
        // (We deliberately do NOT probe lstat: that would diverge from the TS
        // and mis-flag legitimate dir symlinks like macOS /var -> /private/var.)
        match std::fs::metadata(&next_path) {
            Ok(stat) => {
                if stat.is_file() {
                    return true;
                }
            }
            Err(_) => break, // Path doesn't exist — stop checking.
        }
        current_path = next_path;
    }
    false
}

/// Find the first non-existent path component.
///
/// E.g. for `/existing/parent/nonexistent/child/file.txt` where
/// `/existing/parent` exists, returns `/existing/parent/nonexistent`.
///
/// Used to block creation of non-existent deny paths by mounting `/dev/null` at
/// the first missing component.
///
/// Ported from `linux-sandbox-utils.js:83-96` (`findFirstNonExistentComponent`).
#[must_use]
pub fn find_first_non_existent_component(target_path: &str) -> String {
    let mut current_path = String::new();
    for part in target_path.split('/') {
        if part.is_empty() {
            continue; // Skip empty parts (leading /)
        }
        let next_path = format!("{current_path}/{part}");
        if !Path::new(&next_path).exists() {
            return next_path;
        }
        current_path = next_path;
    }
    target_path.to_string() // Shouldn't reach here if called correctly.
}

/// POSIX `path.resolve(base, p)` where `base` is absolute: join (unless `p` is
/// absolute) and collapse `.`/`..`, returning an absolute path with no trailing
/// slash. Mirrors `path_utils`'s private resolver (kept local to avoid widening
/// that module's API).
fn posix_resolve(base: &str, p: &str) -> String {
    let combined = if p.starts_with('/') {
        p.to_string()
    } else {
        format!("{}/{}", base.trim_end_matches('/'), p)
    };
    let is_absolute = combined.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for segment in combined.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if let Some(last) = parts.last() {
                    if *last != ".." {
                        parts.pop();
                        continue;
                    }
                }
                if !is_absolute {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    if is_absolute {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

/// POSIX `path.dirname`.
fn posix_dirname(p: &str) -> String {
    let normalized = p.trim_end_matches('/');
    match normalized.rfind('/') {
        None => ".".to_string(),
        Some(0) => "/".to_string(),
        Some(idx) => normalized[..idx].to_string(),
    }
}

/// POSIX `path.basename` (final path component).
fn posix_basename(p: &str) -> String {
    let normalized = p.trim_end_matches('/');
    match normalized.rfind('/') {
        None => normalized.to_string(),
        Some(idx) => normalized[idx + 1..].to_string(),
    }
}

/// Join `base` with an absolute-path component (`path.join(base, comp)` for an
/// absolute `base`). Empty `base` (from `trim_end_matches('/')` of `/`) yields a
/// leading-slash join.
fn posix_join(base: &str, comp: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), comp)
}

/// Join `base` with each of `comps` in order.
fn posix_join_all(base: &str, comps: &[String]) -> String {
    let mut out = base.trim_end_matches('/').to_string();
    for c in comps {
        out.push('/');
        out.push_str(c);
    }
    out
}

/// Max symlink-chase iterations — 2.1.211 `iYh = 40`.
const MAX_SYMLINK_ITERATIONS: usize = 40;

/// Partial realpath resolver — port of 2.1.211's `sYh`. Resolves the existing
/// prefix of `path` through symlinks (following any symlinked component,
/// including a dangling symlink as the first missing component) while KEEPING a
/// non-existent tail. Returns the resolved path, or `None` when resolution
/// cannot make progress (root reached unresolved, or the symlink chase exceeds
/// [`MAX_SYMLINK_ITERATIONS`]) — the caller then FAILS CLOSED.
///
/// This hardens the Linux deny-within-allow loop: a deny path that IS or crosses
/// a symlink pointing outside the allowed roots is resolved to its real location
/// before the ro-bind, so it cannot dodge the mask (`Resolved symlinked deny
/// path` / fail-closed in 2.1.211; absent in the 0.0.54 / CC≤207 port).
fn partial_realpath(path: &str) -> Option<String> {
    let mut t = path.to_string();
    for _ in 0..MAX_SYMLINK_ITERATIONS {
        // Full path resolves (exists, all symlinks followed) → canonical.
        if let Ok(rp) = std::fs::canonicalize(&t) {
            return Some(rp.to_string_lossy().into_owned());
        }
        // Walk up to the deepest existing ancestor, collecting the missing tail.
        let mut n = t.clone();
        let mut tail: Vec<String> = Vec::new();
        let resolved_ancestor = loop {
            let parent = posix_dirname(&n);
            if parent == n {
                // Reached the root without resolving anything → fail closed.
                return None;
            }
            tail.insert(0, posix_basename(&n));
            n = parent;
            if let Ok(rp) = std::fs::canonicalize(&n) {
                break rp.to_string_lossy().into_owned();
            }
        };
        // `s` = the first missing component under the resolved ancestor.
        let s = posix_join(&resolved_ancestor, &tail[0]);
        match std::fs::read_link(&s) {
            // Not a symlink → resolved ancestor + the full missing tail.
            Err(_) => return Some(posix_join_all(&resolved_ancestor, &tail)),
            // A symlink: resolve it against its own dir, keep the rest of the
            // tail, and loop.
            Ok(link_target) => {
                let base = posix_dirname(&s);
                let resolved = posix_resolve(&base, &link_target.to_string_lossy());
                t = posix_join_all(&resolved, &tail[1..]);
            }
        }
    }
    None
}

/// Get mandatory deny paths using ripgrep (Linux only).
///
/// Uses a SINGLE ripgrep call with multiple glob patterns for efficiency. With
/// `--max-depth` limiting, this is fast enough to run on each command without
/// memoization.
///
/// `ripgrep_cmd` is the ripgrep binary (TS `ripgrepConfig.command`, default
/// `rg`). `cwd` is the search root (the TS reads `process.cwd()`; taking it as a
/// parameter makes this deterministic + testable). When `rg` is absent or errors
/// the scan-match union is simply empty (matching the TS `catch` that logs and
/// continues with the seed-only deny list).
///
/// Ported from `linux-sandbox-utils.js:102-201` (`linuxGetMandatoryDenyPaths`).
#[must_use]
pub fn linux_get_mandatory_deny_paths(
    ripgrep_cmd: &str,
    max_depth: usize,
    allow_git_config: bool,
    cwd: &str,
) -> Vec<String> {
    let dangerous_directories = get_dangerous_directories();

    // Seed: dangerous files + dangerous dirs resolved against cwd.
    // (linux-sandbox-utils.js:109-114)
    let mut deny_paths: Vec<String> = Vec::new();
    for f in DANGEROUS_FILES {
        deny_paths.push(posix_resolve(cwd, f));
    }
    for d in &dangerous_directories {
        deny_paths.push(posix_resolve(cwd, d));
    }

    // Git hooks/config only denied when <cwd>/.git is a directory.
    // (linux-sandbox-utils.js:120-135)
    let dot_git_path = posix_resolve(cwd, ".git");
    let dot_git_is_directory = std::fs::metadata(&dot_git_path).is_ok_and(|m| m.is_dir());
    if dot_git_is_directory {
        deny_paths.push(posix_resolve(cwd, ".git/hooks"));
        if !allow_git_config {
            deny_paths.push(posix_resolve(cwd, ".git/config"));
        }
    }

    // Build iglob args for all patterns in one ripgrep call.
    // (linux-sandbox-utils.js:136-149)
    let mut iglob_args: Vec<String> = Vec::new();
    for file_name in DANGEROUS_FILES {
        iglob_args.push("--iglob".to_string());
        iglob_args.push(file_name.to_string());
    }
    for dir_name in &dangerous_directories {
        iglob_args.push("--iglob".to_string());
        iglob_args.push(format!("**/{dir_name}/**"));
    }
    iglob_args.push("--iglob".to_string());
    iglob_args.push("**/.git/hooks/**".to_string());
    if !allow_git_config {
        iglob_args.push("--iglob".to_string());
        iglob_args.push("**/.git/config".to_string());
    }

    // Single ripgrep call. (linux-sandbox-utils.js:153-167)
    let matches: Vec<String> = run_ripgrep(ripgrep_cmd, max_depth, &iglob_args, cwd);

    // Process matches. (linux-sandbox-utils.js:169-199)
    // dangerousDirectories ++ ['.git'] for the dir-membership scan.
    let mut scan_dirs: Vec<String> = dangerous_directories.clone();
    scan_dirs.push(".git".to_string());

    for m in &matches {
        let absolute_path = posix_resolve(cwd, m);
        let mut found_dir = false;
        for dir_name in &scan_dirs {
            let normalized_dir_name = normalize_case_for_comparison(dir_name);
            let segments: Vec<&str> = absolute_path.split('/').collect();
            let dir_index = segments
                .iter()
                .position(|s| normalize_case_for_comparison(s) == normalized_dir_name);
            if let Some(dir_index) = dir_index {
                if dir_name == ".git" {
                    // For .git, we want hooks/ or config, not the whole .git dir.
                    let git_dir = segments[..=dir_index].join("/");
                    if m.contains(".git/hooks") {
                        deny_paths.push(format!("{git_dir}/hooks"));
                    } else if m.contains(".git/config") {
                        deny_paths.push(format!("{git_dir}/config"));
                    }
                } else {
                    deny_paths.push(segments[..=dir_index].join("/"));
                }
                found_dir = true;
                break;
            }
        }
        if !found_dir {
            deny_paths.push(absolute_path);
        }
    }

    // Dedup preserving order ([...new Set(denyPaths)]).
    let mut seen = HashSet::new();
    deny_paths.retain(|p| seen.insert(p.clone()));
    deny_paths
}

/// Run `rg --files --hidden --max-depth N <iglobs> -g !**/node_modules/** ` over
/// `cwd`, returning matched paths (one per line). Errors / a missing `rg` yield
/// an empty Vec, matching the TS `catch` that logs and continues.
///
/// (linux-sandbox-utils.js:153-167; the `ripGrep` util shells the same args.)
fn run_ripgrep(
    ripgrep_cmd: &str,
    max_depth: usize,
    iglob_args: &[String],
    cwd: &str,
) -> Vec<String> {
    let mut command = Command::new(ripgrep_cmd);
    command
        .arg("--files")
        .arg("--hidden")
        .arg("--max-depth")
        .arg(max_depth.to_string());
    for a in iglob_args {
        command.arg(a);
    }
    command.arg("-g").arg("!**/node_modules/**");
    command.current_dir(cwd);

    let Ok(output) = command.output() else {
        // rg not found / failed to spawn -> seed-only (TS logs + continues).
        return Vec::new();
    };
    // rg exits 1 when no files match; that is not an error for our purposes.
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(ToString::to_string)
        .collect()
}

/// Generate filesystem bind-mount arguments for bwrap.
///
/// Returns `(args, mount_points)`: `args` is the flat `--ro-bind`/`--bind`/
/// `--tmpfs` triple vector to pass to bwrap; `mount_points` are the host files
/// bwrap will create for non-existent deny paths, which the caller must clean up
/// (see the module-level divergence note — the TS uses a global `Set` instead).
///
/// `read_config` / `write_config` are the read/write restrictions (`None` =
/// unrestricted). `ripgrep_cmd`/`max_depth`/`allow_git_config`/`cwd` feed
/// [`linux_get_mandatory_deny_paths`].
///
/// Ported from `linux-sandbox-utils.js:527-772` (`generateFilesystemArgs`).
// This is a faithful 1:1 port of one ~245-line TS function. Splitting it into
// sub-functions would obscure the branch-for-branch parity with the reference
// (and the shared mutable state — args, allowed_write_paths, deny_write_args,
// masked_files, mount_points — threads through every branch). Kept whole.
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn generate_filesystem_args(
    read_config: Option<&ReadConfig>,
    write_config: Option<&WriteConfig>,
    ripgrep_cmd: &str,
    max_depth: usize,
    allow_git_config: bool,
    cwd: &str,
) -> (Vec<String>, Vec<PathBuf>) {
    let mut args: Vec<String> = Vec::new();
    // Normalized allowed write paths; populated in the writeConfig block, read
    // again in the denyRead loop to re-bind writes under tmpfs.
    let mut allowed_write_paths: Vec<String> = Vec::new();
    // denyWrite binds buffered and emitted after denyRead processing (flat triples).
    let mut deny_write_args: Vec<String> = Vec::new();
    // Mount points to return for caller cleanup (TS: module-global bwrapMountPoints).
    let mut mount_points: Vec<PathBuf> = Vec::new();

    // --- Initial root mount based on write restrictions (linux-sandbox-utils.js:537-671) ---
    if let Some(write_config) = write_config {
        // Read-only root, then allow writes to specific paths. (:539)
        args.push("--ro-bind".to_string());
        args.push("/".to_string());
        args.push("/".to_string());

        // Allow writes to specific paths. (:541-575)
        for path_pattern in &write_config.allow_only {
            let normalized_path = normalize_path_for_sandbox(path_pattern);
            // Skip /dev/* (handled by --dev /dev). (:545-548)
            if normalized_path.starts_with("/dev/") {
                continue;
            }
            // Skip non-existent. (:549-552)
            if !Path::new(&normalized_path).exists() {
                continue;
            }
            // Skip symlink-outside-boundary via realpath + isSymlinkOutsideBoundary. (:556-572)
            match std::fs::canonicalize(&normalized_path) {
                Ok(resolved) => {
                    let resolved_path = resolved.to_string_lossy().into_owned();
                    // Trim trailing slashes before comparing (realpath never has one). (:561)
                    let normalized_for_comparison =
                        normalized_path.trim_end_matches('/').to_string();
                    if resolved_path != normalized_for_comparison
                        && is_symlink_outside_boundary(&normalized_path, &resolved_path)
                    {
                        continue;
                    }
                }
                Err(_) => {
                    // realpath failed - skip. (:568-571)
                    continue;
                }
            }
            args.push("--bind".to_string());
            args.push(normalized_path.clone());
            args.push(normalized_path.clone());
            allowed_write_paths.push(normalized_path);
        }

        // Deny writes within allowed paths (user-specified + mandatory). (:577-580)
        let mut deny_paths: Vec<String> = write_config.deny_within_allow.clone();
        deny_paths.extend(linux_get_mandatory_deny_paths(
            ripgrep_cmd,
            max_depth,
            allow_git_config,
            cwd,
        ));

        // Deny loop (2.1.211-hardened): resolve each deny path through symlinks
        // and fail closed if resolution can't make progress, so a symlinked deny
        // path can't dodge the ro-bind. Dedup is on the RESOLVED path (and on
        // any mount point emitted). `seen_deny_write` is the 2.1.211 `w` set;
        // `mount_original` is the `d` map (mount point -> original deny path),
        // kept for the mount bookkeeping.
        let mut seen_deny_write: HashSet<String> = HashSet::new();
        let mut mount_original: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for path_pattern in &deny_paths {
            let i_path = normalize_path_for_sandbox(path_pattern);
            // Skip /dev/* BEFORE resolution. (:for x of b … I.startsWith("/dev/"))
            if i_path.starts_with("/dev/") {
                continue;
            }
            // Resolve the deny path through symlinks (partial realpath). On
            // failure, FAIL CLOSED: ro-bind /dev/null at the (allowed-contained)
            // original location and continue.
            let Some(h_path) = partial_realpath(&i_path) else {
                if let Some(symlink_in_path) = find_symlink_in_path(&i_path, &allowed_write_paths) {
                    if seen_deny_write.insert(symlink_in_path.clone()) {
                        deny_write_args.push("--ro-bind".to_string());
                        deny_write_args.push("/dev/null".to_string());
                        deny_write_args.push(symlink_in_path.clone());
                        mount_original.insert(symlink_in_path, i_path.clone());
                    }
                }
                // "[Sandbox Linux] Deny path could not be resolved through
                // symlinks, failing closed: {i_path}"
                continue;
            };
            // (`H !== I` would log "Resolved symlinked deny path: I -> H".)

            // Skip /dev/* AFTER resolution too (a symlink may resolve into /dev).
            if h_path.starts_with("/dev/") {
                continue;
            }
            // Dedup on the resolved path.
            if !seen_deny_write.insert(h_path.clone()) {
                continue;
            }
            // Symlink-in-path within an allowed write path -> mask the symlink
            // location with /dev/null (symlink-replacement-attack guard),
            // checked against the RESOLVED path. (:V6c(H,c))
            if let Some(symlink_in_path) = find_symlink_in_path(&h_path, &allowed_write_paths) {
                if seen_deny_write.insert(symlink_in_path.clone()) {
                    deny_write_args.push("--ro-bind".to_string());
                    deny_write_args.push("/dev/null".to_string());
                    deny_write_args.push(symlink_in_path.clone());
                    mount_original.insert(symlink_in_path, i_path.clone());
                }
                continue;
            }
            // Non-existent resolved paths. (:612-655, on H)
            if !Path::new(&h_path).exists() {
                // Fix 1 (worktree): file ancestor -> skip. (:617-620)
                if has_file_ancestor(&h_path) {
                    continue;
                }
                // Deepest existing ancestor directory. (:622-625)
                let mut ancestor_path = posix_dirname(&h_path);
                while ancestor_path != "/" && !Path::new(&ancestor_path).exists() {
                    ancestor_path = posix_dirname(&ancestor_path);
                }
                // Only protect if existing ancestor is within an allowed write path. (:628-630)
                let ancestor_is_within_allowed_path = allowed_write_paths.iter().any(|allowed| {
                    ancestor_path.starts_with(&format!("{allowed}/"))
                        || ancestor_path == *allowed
                        || h_path.starts_with(&format!("{allowed}/"))
                });
                if ancestor_is_within_allowed_path {
                    let first_non_existent = find_first_non_existent_component(&h_path);
                    // TS: `if (firstNonExistent !== H) { emptyDir } else { /dev/null }`.
                    // Inverted here (== first) to satisfy clippy::if_not_else.
                    if first_non_existent == h_path {
                        // Leaf -> /dev/null. (:644-649)
                        deny_write_args.push("--ro-bind".to_string());
                        deny_write_args.push("/dev/null".to_string());
                        deny_write_args.push(first_non_existent.clone());
                        mount_original.insert(first_non_existent.clone(), i_path.clone());
                        mount_points.push(PathBuf::from(first_non_existent));
                    } else {
                        // Fix 2: intermediate component -> empty dir mount. (:637-643)
                        let empty_dir = make_empty_dir();
                        deny_write_args.push("--ro-bind".to_string());
                        deny_write_args.push(empty_dir);
                        deny_write_args.push(first_non_existent.clone());
                        mount_original.insert(first_non_existent.clone(), i_path.clone());
                        mount_points.push(PathBuf::from(first_non_existent));
                    }
                }
                // else: not within allowed -> already read-only, skip. (:651-653)
                continue;
            }
            // Existent resolved path within allowed write path -> ro-bind H H. (:658-665)
            let is_within_allowed_path = allowed_write_paths
                .iter()
                .any(|allowed| h_path.starts_with(&format!("{allowed}/")) || h_path == *allowed);
            if is_within_allowed_path {
                deny_write_args.push("--ro-bind".to_string());
                deny_write_args.push(h_path.clone());
                deny_write_args.push(h_path.clone());
                mount_original.insert(h_path.clone(), i_path.clone());
            }
            // else: outside allowed -> already read-only, skip.
        }
        // `mount_original` (2.1.211 `d`) currently backs the mount bookkeeping;
        // the 2.1.211 denyWrite flush additionally consults it for the
        // denyRead-tmpfs re-exposure check (`S(x)`/`E`), which is a separate,
        // not-yet-ported parity item — see the flush below.
        let _ = &mount_original;
    } else {
        // No write restrictions: allow all writes. (:668-671)
        args.push("--bind".to_string());
        args.push("/".to_string());
        args.push("/".to_string());
    }

    // --- Read restrictions: tmpfs over denied paths (linux-sandbox-utils.js:673-760) ---
    let mut read_deny_paths: Vec<String> = Vec::new();
    let read_allow_paths: Vec<String> = read_config
        .map(|c| {
            c.allow_within_deny
                .iter()
                .map(|p| normalize_path_for_sandbox(p))
                .collect()
        })
        .unwrap_or_default();
    // Files masked by --ro-bind /dev/null below. Filters denyWriteArgs so a
    // re-bind doesn't undo the mask. (:678)
    let mut masked_files: HashSet<String> = HashSet::new();

    // Expand a root deny into its direct children (skip proc/dev/sys). (:684-695)
    let root_skip: HashSet<&str> = ["proc", "dev", "sys"].into_iter().collect();
    if let Some(read_config) = read_config {
        for p in &read_config.deny_only {
            if normalize_path_for_sandbox(p) == "/" {
                if let Ok(entries) = std::fs::read_dir("/") {
                    for entry in entries.flatten() {
                        let child = entry.file_name().to_string_lossy().into_owned();
                        if !root_skip.contains(child.as_str()) {
                            read_deny_paths.push(format!("/{child}"));
                        }
                    }
                }
            } else {
                read_deny_paths.push(p.clone());
            }
        }
    }

    // Always hide /etc/ssh/ssh_config.d if it exists. (:699-701)
    if Path::new("/etc/ssh/ssh_config.d").exists() {
        read_deny_paths.push("/etc/ssh/ssh_config.d".to_string());
    }

    // Normalize then sort shallow-first (by component count). (:705-707)
    let mut normalized_deny_paths: Vec<String> = read_deny_paths
        .iter()
        .map(|p| normalize_path_for_sandbox(p))
        .collect();
    normalized_deny_paths.sort_by_key(|p| p.split('/').count());

    for normalized_path in &normalized_deny_paths {
        if !Path::new(normalized_path).exists() {
            continue; // Skip non-existent read deny path. (:709-712)
        }
        let deny_sep = if normalized_path == "/" {
            "/".to_string()
        } else {
            format!("{normalized_path}/")
        };
        let Ok(read_deny_stat) = std::fs::metadata(normalized_path) else {
            continue;
        };
        if read_deny_stat.is_dir() {
            args.push("--tmpfs".to_string());
            args.push(normalized_path.clone());
            // Restore write binds wiped by the tmpfs. (:718-723)
            for write_path in &allowed_write_paths {
                if write_path.starts_with(&deny_sep) || write_path == normalized_path {
                    args.push("--bind".to_string());
                    args.push(write_path.clone());
                    args.push(write_path.clone());
                }
            }
            // Re-allow specific paths within the denied directory. (:727-745)
            for allow_path in &read_allow_paths {
                if allow_path.starts_with(&deny_sep) || allow_path == normalized_path {
                    if !Path::new(allow_path).exists() {
                        continue; // (:729-732)
                    }
                    // Skip only if a write path re-bound above AND covers allowPath. (:737-740)
                    let covered_by_rebound_write = allowed_write_paths.iter().any(|w| {
                        (w.starts_with(&deny_sep) || w == normalized_path)
                            && (allow_path == w || allow_path.starts_with(&format!("{w}/")))
                    });
                    if covered_by_rebound_write {
                        continue;
                    }
                    args.push("--ro-bind".to_string());
                    args.push(allow_path.clone());
                    args.push(allow_path.clone());
                }
            }
        } else {
            // File: only an exact allowRead match overrides the deny. (:748-755)
            if read_allow_paths.contains(normalized_path) {
                continue;
            }
            // Bind /dev/null instead of tmpfs. (:756-758)
            args.push("--ro-bind".to_string());
            args.push("/dev/null".to_string());
            args.push(normalized_path.clone());
            masked_files.insert(normalized_path.clone());
        }
    }

    // Emit buffered denyWrite LAST, skipping any dest already masked. (:766-771)
    let mut i = 0;
    while i < deny_write_args.len() {
        let dest = &deny_write_args[i + 2];
        if !masked_files.contains(dest) {
            args.push(deny_write_args[i].clone());
            args.push(deny_write_args[i + 1].clone());
            args.push(deny_write_args[i + 2].clone());
        }
        i += 3;
    }

    (args, mount_points)
}

/// Create an empty temp dir under the system temp dir, mirroring the TS
/// `fs.mkdtempSync(path.join(tmpdir(), 'claude-empty-'))`. Returns the path. On
/// failure (extremely rare) falls back to `/dev/null` so the deny still applies
/// as a file mask rather than panicking — bwrap then masks with a char device.
fn make_empty_dir() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    // Per-process monotonic counter so repeated calls never collide even within
    // the same nanosecond.
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let base = std::env::temp_dir();
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    for _ in 0..16 {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = base.join(format!("claude-empty-{pid}-{nanos}-{n}"));
        if std::fs::create_dir(&candidate).is_ok() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    "/dev/null".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    /// Raw tempdir path joined with `rel` (no symlink resolution). Used by the
    /// helper + mandatory-deny tests, which build expectations from
    /// `posix_resolve` over the raw cwd (a pure string op).
    fn p(dir: &TempDir, rel: &str) -> String {
        dir.path().join(rel).to_string_lossy().into_owned()
    }

    /// CANONICAL tempdir path joined with `rel`. On macOS `/var/folders/...` is
    /// a symlink to `/private/var/...`, and `normalize_path_for_sandbox`
    /// (which `generate_filesystem_args` runs on every input) resolves it — so
    /// the emitted arg paths are canonical. The `generate_*` tests build their
    /// expectations through `cp` so they match regardless of platform.
    fn cp(dir: &TempDir, rel: &str) -> String {
        let base = fs::canonicalize(dir.path()).unwrap();
        base.join(rel).to_string_lossy().into_owned()
    }

    /// Canonical tempdir root as a String (for `allow_only` inputs).
    fn canon(dir: &TempDir) -> String {
        fs::canonicalize(dir.path())
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    /// Find the index `i` where `args[i..i+3]` equals the given triple.
    fn triple_index(args: &[String], a: &str, b: &str, c: &str) -> Option<usize> {
        (0..args.len().saturating_sub(2))
            .find(|&i| args[i] == a && args[i + 1] == b && args[i + 2] == c)
    }

    fn has_triple(args: &[String], a: &str, b: &str, c: &str) -> bool {
        triple_index(args, a, b, c).is_some()
    }

    // --- Helper tests (linux-sandbox-utils.js:21-100) ---

    #[test]
    fn find_symlink_in_path_detects_symlink_within_allowed() {
        let dir = TempDir::new().unwrap();
        // <dir>/link -> <dir>/real ; target path <dir>/link/settings.json
        fs::create_dir(dir.path().join("real")).unwrap();
        symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
        let allowed = vec![dir.path().to_string_lossy().into_owned()];
        let target = p(&dir, "link/settings.json");
        let found = find_symlink_in_path(&target, &allowed);
        assert_eq!(found, Some(p(&dir, "link")));
    }

    #[test]
    fn find_symlink_in_path_none_when_no_symlink() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("plain")).unwrap();
        let allowed = vec![dir.path().to_string_lossy().into_owned()];
        let target = p(&dir, "plain/file.txt");
        assert_eq!(find_symlink_in_path(&target, &allowed), None);
    }

    #[test]
    fn find_symlink_in_path_none_when_symlink_outside_allowed() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("real")).unwrap();
        symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
        // allowed write paths do NOT include this dir.
        let allowed = vec!["/some/other/allowed".to_string()];
        let target = p(&dir, "link/settings.json");
        assert_eq!(find_symlink_in_path(&target, &allowed), None);
    }

    #[test]
    fn has_file_ancestor_true_for_git_file_worktree() {
        let dir = TempDir::new().unwrap();
        // .git is a FILE (worktree case): .git/hooks can never be created.
        fs::write(dir.path().join(".git"), "gitdir: /elsewhere").unwrap();
        let target = p(&dir, ".git/hooks");
        assert!(has_file_ancestor(&target));
    }

    #[test]
    fn has_file_ancestor_false_for_git_dir() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        let target = p(&dir, ".git/hooks");
        assert!(!has_file_ancestor(&target));
    }

    #[test]
    fn find_first_non_existent_component_returns_first_missing() {
        let dir = TempDir::new().unwrap();
        // <dir>/existing exists; <dir>/existing/nope/child/file.txt does not.
        fs::create_dir(dir.path().join("existing")).unwrap();
        let target = p(&dir, "existing/nope/child/file.txt");
        let expected = p(&dir, "existing/nope");
        assert_eq!(find_first_non_existent_component(&target), expected);
    }

    // --- partial_realpath / sYh (2.1.211 symlink-resolving deny hardening) ---

    #[test]
    fn partial_realpath_resolves_existing_path() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let path = dir.path().join("sub").to_string_lossy().into_owned();
        let expected = fs::canonicalize(dir.path().join("sub"))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(partial_realpath(&path), Some(expected));
    }

    #[test]
    fn partial_realpath_keeps_nonexistent_tail() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("exists")).unwrap();
        let path = dir
            .path()
            .join("exists/no/tail")
            .to_string_lossy()
            .into_owned();
        let base = fs::canonicalize(dir.path().join("exists"))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(partial_realpath(&path), Some(format!("{base}/no/tail")));
    }

    #[test]
    fn partial_realpath_follows_symlinked_component() {
        let dir = TempDir::new().unwrap();
        // link -> real (both exist); target link/missing keeps the missing tail
        // but resolves link -> real.
        fs::create_dir(dir.path().join("real")).unwrap();
        symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
        let path = dir
            .path()
            .join("link/missing")
            .to_string_lossy()
            .into_owned();
        let real = fs::canonicalize(dir.path().join("real"))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(partial_realpath(&path), Some(format!("{real}/missing")));
    }

    #[test]
    fn partial_realpath_none_on_symlink_loop() {
        let dir = TempDir::new().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        // a -> b, b -> a : an unresolvable loop → fail closed (None).
        symlink(base.join("b"), base.join("a")).unwrap();
        symlink(base.join("a"), base.join("b")).unwrap();
        let path = base.join("a/x").to_string_lossy().into_owned();
        assert_eq!(partial_realpath(&path), None);
    }

    #[test]
    fn deny_within_allow_follows_symlink_to_resolved_path() {
        // PERM-DELTA-SBX-SYMLINK-DENY-04: a deny path crossing a symlink is
        // resolved through the symlink before the ro-bind, so it masks the REAL
        // location (not the symlink), and does not leave the resolved file
        // writable.
        let allow = TempDir::new().unwrap();
        let allow_path = canon(&allow);
        fs::create_dir(allow.path().join("real")).unwrap();
        fs::write(allow.path().join("real/secret"), "x").unwrap();
        symlink(allow.path().join("real"), allow.path().join("link")).unwrap();
        let deny = cp(&allow, "link/secret");
        let resolved = fs::canonicalize(allow.path().join("link/secret"))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let cwd = empty_cwd();
        let wc = WriteConfig {
            allow_only: vec![allow_path.clone()],
            deny_within_allow: vec![deny.clone()],
        };
        let (args, _mp) = generate_filesystem_args(
            None,
            Some(&wc),
            "rg",
            3,
            false,
            &cwd.path().to_string_lossy(),
        );
        // The RESOLVED real path is ro-bound (deny applied at the real location).
        assert!(
            has_triple(&args, "--ro-bind", &resolved, &resolved),
            "resolved deny path must be ro-bound; args={args:?}"
        );
        // The symlink itself is NOT the mask target (old, pre-resolution behavior).
        let link = cp(&allow, "link");
        assert!(
            !has_triple(&args, "--ro-bind", "/dev/null", &link),
            "symlink location must not be the /dev/null mask after resolution; args={args:?}"
        );
    }

    // --- linux_get_mandatory_deny_paths (linux-sandbox-utils.js:102-201) ---

    fn rg_available() -> bool {
        Command::new("rg")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn mandatory_deny_paths_seed_and_ripgrep() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().to_string_lossy().into_owned();

        // .git as a directory -> hooks + config are denied.
        fs::create_dir(dir.path().join(".git")).unwrap();
        // A nested dangerous file the ripgrep scan should find.
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/.env"), "SECRET=1").unwrap();
        // A nested .gitconfig (in DANGEROUS_FILES) the scan should find.
        fs::write(dir.path().join("sub/.gitconfig"), "x").unwrap();

        let out = linux_get_mandatory_deny_paths("rg", 3, false, &cwd);

        // Seed: top-level dangerous files resolved against cwd.
        assert!(out.contains(&p(&dir, ".gitconfig")));
        assert!(out.contains(&p(&dir, ".mcp.json")));
        // Seed: dangerous directories.
        assert!(out.contains(&p(&dir, ".vscode")));
        assert!(out.contains(&p(&dir, ".lingxi/commands")));
        // .git is a dir -> hooks + config denied.
        assert!(out.contains(&p(&dir, ".git/hooks")));
        assert!(out.contains(&p(&dir, ".git/config")));
        // Dedup: no duplicates.
        let mut sorted = out.clone();
        sorted.sort();
        let len_before = sorted.len();
        sorted.dedup();
        assert_eq!(len_before, sorted.len(), "deny paths must be deduped");

        if rg_available() {
            // ripgrep found the nested dangerous files.
            assert!(
                out.contains(&p(&dir, "sub/.gitconfig")),
                "ripgrep should surface nested .gitconfig; got {out:?}"
            );
            // .env is not in DANGEROUS_FILES, so it should NOT appear (the iglob
            // patterns are DANGEROUS_FILES + dirs only).
        } else {
            eprintln!("SKIP: rg not on PATH, ripgrep scan branch not exercised");
        }
    }

    #[test]
    fn mandatory_deny_no_git_hooks_when_git_is_file() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().to_string_lossy().into_owned();
        // .git is a FILE (worktree) -> hooks/config NOT added from seed.
        fs::write(dir.path().join(".git"), "gitdir: /x").unwrap();
        let out = linux_get_mandatory_deny_paths("rg", 3, false, &cwd);
        assert!(!out.contains(&p(&dir, ".git/hooks")));
        assert!(!out.contains(&p(&dir, ".git/config")));
    }

    #[test]
    fn mandatory_deny_allow_git_config_keeps_hooks_drops_config() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().to_string_lossy().into_owned();
        fs::create_dir(dir.path().join(".git")).unwrap();
        let out = linux_get_mandatory_deny_paths("rg", 3, true, &cwd);
        assert!(out.contains(&p(&dir, ".git/hooks")));
        assert!(
            !out.contains(&p(&dir, ".git/config")),
            "allow_git_config=true must NOT deny .git/config"
        );
    }

    #[test]
    fn mandatory_deny_missing_rg_is_seed_only() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().to_string_lossy().into_owned();
        // Bogus ripgrep binary -> scan yields nothing, seed still present.
        let out = linux_get_mandatory_deny_paths("definitely-not-a-real-rg-binary", 3, false, &cwd);
        assert!(out.contains(&p(&dir, ".gitconfig")));
    }

    // --- generate_filesystem_args matrix (linux-sandbox-utils.js:527-772) ---
    //
    // To keep the write-deny tests focused on the branch under test, we point
    // cwd at an empty tempdir with no .git so linux_get_mandatory_deny_paths
    // contributes only top-level dangerous-file seeds (all non-existent, all
    // outside the allowed write path -> skipped).

    fn empty_cwd() -> TempDir {
        TempDir::new().unwrap()
    }

    #[test]
    fn case_a_write_restrict_root_allow_dir() {
        let allow = TempDir::new().unwrap();
        let allow_path = canon(&allow);
        let cwd = empty_cwd();
        let wc = WriteConfig {
            allow_only: vec![allow_path.clone()],
            deny_within_allow: vec![],
        };
        let (args, _mp) = generate_filesystem_args(
            None,
            Some(&wc),
            "rg",
            3,
            false,
            &cwd.path().to_string_lossy(),
        );
        // --ro-bind / / first, then --bind <allow> <allow>.
        let root_idx = triple_index(&args, "--ro-bind", "/", "/").expect("ro-bind / /");
        let bind_idx =
            triple_index(&args, "--bind", &allow_path, &allow_path).expect("bind allow dir");
        assert!(
            root_idx < bind_idx,
            "ro-bind / / must precede the allow bind"
        );
    }

    #[test]
    fn case_b_deny_within_allow_existent() {
        let allow = TempDir::new().unwrap();
        let allow_path = canon(&allow);
        // An existent file inside the allowed dir to deny.
        fs::create_dir(allow.path().join("secret")).unwrap();
        let deny_path = cp(&allow, "secret");
        let cwd = empty_cwd();
        let wc = WriteConfig {
            allow_only: vec![allow_path.clone()],
            deny_within_allow: vec![deny_path.clone()],
        };
        let (args, _mp) = generate_filesystem_args(
            None,
            Some(&wc),
            "rg",
            3,
            false,
            &cwd.path().to_string_lossy(),
        );
        let bind_idx = triple_index(&args, "--bind", &allow_path, &allow_path).expect("allow bind");
        let deny_idx =
            triple_index(&args, "--ro-bind", &deny_path, &deny_path).expect("deny ro-bind p p");
        assert!(
            bind_idx < deny_idx,
            "deny ro-bind must come AFTER allow bind"
        );
    }

    #[test]
    fn case_c_nonexistent_leaf_deny_within_allowed() {
        let allow = TempDir::new().unwrap();
        let allow_path = canon(&allow);
        // Leaf doesn't exist but its parent (allow dir) does.
        let deny_leaf = cp(&allow, ".bashrc");
        let cwd = empty_cwd();
        let wc = WriteConfig {
            allow_only: vec![allow_path.clone()],
            deny_within_allow: vec![deny_leaf.clone()],
        };
        let (args, mp) = generate_filesystem_args(
            None,
            Some(&wc),
            "rg",
            3,
            false,
            &cwd.path().to_string_lossy(),
        );
        assert!(
            has_triple(&args, "--ro-bind", "/dev/null", &deny_leaf),
            "non-existent leaf -> /dev/null mask; args={args:?}"
        );
        assert!(
            mp.contains(&PathBuf::from(&deny_leaf)),
            "leaf mount point recorded; mp={mp:?}"
        );
    }

    #[test]
    fn case_d_nonexistent_intermediate_deny() {
        let allow = TempDir::new().unwrap();
        let allow_path = canon(&allow);
        // Neither <allow>/missing nor <allow>/missing/config exist.
        let deny_path = cp(&allow, "missing/config");
        let first_non_existent = cp(&allow, "missing");
        let cwd = empty_cwd();
        let wc = WriteConfig {
            allow_only: vec![allow_path.clone()],
            deny_within_allow: vec![deny_path.clone()],
        };
        let (args, mp) = generate_filesystem_args(
            None,
            Some(&wc),
            "rg",
            3,
            false,
            &cwd.path().to_string_lossy(),
        );
        // Intermediate component -> --ro-bind <emptydir> <missing>. The empty dir
        // is a temp path, so match the dest + verb only.
        let idx = (0..args.len().saturating_sub(2)).find(|&i| {
            args[i] == "--ro-bind"
                && args[i + 2] == first_non_existent
                && args[i + 1] != "/dev/null"
        });
        assert!(
            idx.is_some(),
            "intermediate -> --ro-bind <emptydir> <component>; args={args:?}"
        );
        assert!(mp.contains(&PathBuf::from(&first_non_existent)));
    }

    #[test]
    fn case_e_file_ancestor_deny_skipped() {
        let allow = TempDir::new().unwrap();
        let allow_path = canon(&allow);
        // .git is a FILE inside the allowed dir; deny .git/hooks -> SKIPPED.
        fs::write(allow.path().join(".git"), "gitdir: /x").unwrap();
        let deny_path = cp(&allow, ".git/hooks");
        let cwd = empty_cwd();
        let wc = WriteConfig {
            allow_only: vec![allow_path.clone()],
            deny_within_allow: vec![deny_path.clone()],
        };
        let (args, mp) = generate_filesystem_args(
            None,
            Some(&wc),
            "rg",
            3,
            false,
            &cwd.path().to_string_lossy(),
        );
        // No mount for the .git/hooks deny, and nothing references it.
        assert!(
            !args.iter().any(|a| a == &deny_path),
            "file-ancestor deny must be skipped; args={args:?}"
        );
        assert!(!mp.contains(&PathBuf::from(&deny_path)));
    }

    #[test]
    fn case_f_deny_read_dir_tmpfs_and_rebinds() {
        let root = TempDir::new().unwrap();
        // denyRead dir = <root>/deny ; a write path under it; an allowRead under it.
        fs::create_dir(root.path().join("deny")).unwrap();
        fs::create_dir(root.path().join("deny/writable")).unwrap();
        fs::create_dir(root.path().join("deny/readable")).unwrap();
        let deny_dir = cp(&root, "deny");
        let write_sub = cp(&root, "deny/writable");
        let allow_sub = cp(&root, "deny/readable");
        let cwd = empty_cwd();
        let wc = WriteConfig {
            allow_only: vec![write_sub.clone()],
            deny_within_allow: vec![],
        };
        let rc = ReadConfig {
            deny_only: vec![deny_dir.clone()],
            allow_within_deny: vec![allow_sub.clone()],
        };
        let (args, _mp) = generate_filesystem_args(
            Some(&rc),
            Some(&wc),
            "rg",
            3,
            false,
            &cwd.path().to_string_lossy(),
        );
        // tmpfs over the denied dir.
        let tmpfs_idx = (0..args.len().saturating_sub(1))
            .find(|&i| args[i] == "--tmpfs" && args[i + 1] == deny_dir)
            .expect("tmpfs over deny dir");
        // The write path is bound both initially (in the writeConfig block) AND
        // re-bound AFTER the tmpfs — assert a re-bind triple exists past tmpfs_idx.
        let rebind_after = (tmpfs_idx..args.len().saturating_sub(2))
            .any(|i| args[i] == "--bind" && args[i + 1] == write_sub && args[i + 2] == write_sub);
        assert!(
            rebind_after,
            "write path must be re-bound after tmpfs; args={args:?}"
        );
        // allowRead re-bound under tmpfs (its only occurrence is after the tmpfs).
        let allow_idx =
            triple_index(&args, "--ro-bind", &allow_sub, &allow_sub).expect("re-bind allowRead");
        assert!(tmpfs_idx < allow_idx);
    }

    #[test]
    fn case_g_deny_read_file_dev_null() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("secret.env"), "S=1").unwrap();
        let deny_file = cp(&root, "secret.env");
        let cwd = empty_cwd();
        let rc = ReadConfig {
            deny_only: vec![deny_file.clone()],
            allow_within_deny: vec![],
        };
        let (args, _mp) = generate_filesystem_args(
            Some(&rc),
            None, // no write config
            "rg",
            3,
            false,
            &cwd.path().to_string_lossy(),
        );
        assert!(
            has_triple(&args, "--ro-bind", "/dev/null", &deny_file),
            "denyRead file -> /dev/null; args={args:?}"
        );
    }

    #[test]
    fn case_g2_deny_read_file_exact_allow_match_skips() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("keep.env"), "S=1").unwrap();
        let deny_file = cp(&root, "keep.env");
        let cwd = empty_cwd();
        let rc = ReadConfig {
            deny_only: vec![deny_file.clone()],
            // exact file match in allowWithinDeny -> un-deny.
            allow_within_deny: vec![deny_file.clone()],
        };
        let (args, _mp) = generate_filesystem_args(
            Some(&rc),
            None,
            "rg",
            3,
            false,
            &cwd.path().to_string_lossy(),
        );
        assert!(
            !has_triple(&args, "--ro-bind", "/dev/null", &deny_file),
            "exact allowRead file match must skip the deny; args={args:?}"
        );
    }

    #[test]
    fn case_h_deny_write_masked_by_deny_read_not_reemitted() {
        // A path that is BOTH a denyWrite (existent, within allowed) AND a
        // denyRead file. The denyRead /dev/null mask lands first; the buffered
        // denyWrite ro-bind p p must be skipped so it doesn't undo the mask.
        let allow = TempDir::new().unwrap();
        let allow_path = canon(&allow);
        fs::write(allow.path().join("dual.conf"), "x").unwrap();
        let dual = cp(&allow, "dual.conf");
        let cwd = empty_cwd();
        let wc = WriteConfig {
            allow_only: vec![allow_path.clone()],
            deny_within_allow: vec![dual.clone()],
        };
        let rc = ReadConfig {
            deny_only: vec![dual.clone()],
            allow_within_deny: vec![],
        };
        let (args, _mp) = generate_filesystem_args(
            Some(&rc),
            Some(&wc),
            "rg",
            3,
            false,
            &cwd.path().to_string_lossy(),
        );
        // denyRead masked it with /dev/null.
        assert!(has_triple(&args, "--ro-bind", "/dev/null", &dual));
        // denyWrite ro-bind <dual> <dual> must NOT be present.
        assert!(
            !has_triple(&args, "--ro-bind", &dual, &dual),
            "masked denyWrite must not be re-emitted; args={args:?}"
        );
    }

    #[test]
    fn case_i_no_write_config_binds_root_rw() {
        let cwd = empty_cwd();
        let (args, mp) =
            generate_filesystem_args(None, None, "rg", 3, false, &cwd.path().to_string_lossy());
        assert!(has_triple(&args, "--bind", "/", "/"));
        assert!(!has_triple(&args, "--ro-bind", "/", "/"));
        assert!(mp.is_empty());
    }

    #[test]
    fn empty_allow_only_denies_all_writes() {
        // EMPTY allow_only = deny ALL writes: ro-bind / / with no allow binds.
        let cwd = empty_cwd();
        let wc = WriteConfig::default();
        let (args, _mp) = generate_filesystem_args(
            None,
            Some(&wc),
            "rg",
            3,
            false,
            &cwd.path().to_string_lossy(),
        );
        assert!(has_triple(&args, "--ro-bind", "/", "/"));
        assert!(!has_triple(&args, "--bind", "/", "/"));
    }

    #[test]
    fn config_serde_camel_case() {
        // Confirms the TS field names (allowOnly/denyWithinAllow/denyOnly/allowWithinDeny).
        let wc: WriteConfig =
            serde_json::from_str(r#"{"allowOnly":["/a"],"denyWithinAllow":["/a/b"]}"#).unwrap();
        assert_eq!(wc.allow_only, vec!["/a".to_string()]);
        assert_eq!(wc.deny_within_allow, vec!["/a/b".to_string()]);
        let rc: ReadConfig =
            serde_json::from_str(r#"{"denyOnly":["/c"],"allowWithinDeny":["/c/d"]}"#).unwrap();
        assert_eq!(rc.deny_only, vec!["/c".to_string()]);
        assert_eq!(rc.allow_within_deny, vec!["/c/d".to_string()]);
    }
}
