//! Port of `convertToSandboxRuntimeConfig` and `getLinuxGlobPatternWarnings`
//! from `claude-code/src/utils/sandbox/sandbox-adapter.ts`.
//!
//! Walks `SettingsJson.permissions.allow/deny` for `Edit(...)`, `Read(...)`,
//! `WebFetch(domain:...)` rules and folds the extracted patterns into a
//! `SandboxRuntimeConfig`. `Bash(...)` rules are intentionally ignored here —
//! the bash decision (compound + env-var stripping + excludedCommands match)
//! lives in [`crate::decision::should_use_sandbox`].

use crate::path_pattern::resolve_path_pattern_for_sandbox;
use crate::runtime_config::{
    FilesystemRestrictionConfig, NetworkRestrictionConfig, RipgrepConfig, SandboxRuntimeConfig,
    SettingsJson,
};
use std::path::{Path, PathBuf};

/// Tool-name prefix matchers. claude-code's permission rules carry the tool
/// name as `Edit`, `Read`, `Bash`, or `WebFetch` — we hard-code those.
const TOOL_EDIT: &str = "Edit";
const TOOL_READ: &str = "Read";
const TOOL_WEBFETCH: &str = "WebFetch";

/// Session/host-side seeds that claude-code's `convertToSandboxRuntimeConfig`
/// reads from module-level state (`getClaudeTempDir`, `SETTING_SOURCES`,
/// `getManagedSettingsDropInDir`, `getCwdState`, `getAdditionalDirectoriesForLingxiMd`,
/// `worktreeMainRepoPath`, …). In Rust we have no global session object, so the
/// caller threads these in explicitly. All fields default to empty/`None`, so a
/// minimal caller passes `&SandboxConvertContext::default()` and gets the same
/// result as the no-seed path (only `.` is seeded into `allow_write`).
///
/// Mirrors the seed application in sandbox-adapter.ts:225-299.
#[derive(Debug, Clone, Default)]
pub struct SandboxConvertContext {
    /// Protect the registry-owned task output directory against child writes.
    pub task_output_dir: Option<String>,
    /// Claude temp dir (`getClaudeTempDir()`), seeded into `allow_write` right
    /// after `.` (sandbox-adapter.ts:225). Needed for Shell.ts cwd tracking
    /// files. `None` => not seeded (only `.`).
    pub lingxi_temp_dir: Option<String>,
    /// Resolved settings-file paths across all sources
    /// (`SETTING_SOURCES.map(getSettingsFilePathForSource)`), each unconditionally
    /// added to `deny_write` to prevent sandbox escape (sandbox-adapter.ts:232-235).
    pub settings_file_paths: Vec<String>,
    /// Managed-settings drop-in dir (`getManagedSettingsDropInDir()`), added to
    /// `deny_write` (sandbox-adapter.ts:236).
    pub managed_drop_in_dir: Option<String>,
    /// `.lingxi/settings*.json` + `.lingxi/skills` paths derived from the
    /// current working directory when it differs from the original cwd, added to
    /// `deny_write` (sandbox-adapter.ts:238-255). Threaded pre-resolved.
    pub cwd_settings_paths: Vec<String>,
    /// `.lingxi/skills` paths for the original (and current) cwd, added to
    /// `deny_write` (sandbox-adapter.ts:247-255). Threaded pre-resolved.
    pub skills_dirs: Vec<String>,
    /// Cached git-worktree main repo path (`worktreeMainRepoPath`), added to
    /// `allow_write` when present and `!= cwd` (sandbox-adapter.ts:286-288).
    pub worktree_main_repo_path: Option<String>,
    /// Session-only `--add-dir` / `/add-dir` directories
    /// (`getAdditionalDirectoriesForLingxiMd()`), unioned with
    /// `permissions.additionalDirectories` into `allow_write`
    /// (sandbox-adapter.ts:295-299).
    pub additional_md_dirs: Vec<String>,
    /// `allowManagedDomainsOnly` enforcement (sandbox-adapter.ts: the per-source
    /// merge that drops lower-source `allowedDomains` when managed settings set
    /// the flag). `Some(domains)` ⇒ the flag is active in MANAGED settings, and
    /// `domains` is the MANAGED-source allowlist (managed
    /// `sandbox.network.allowedDomains` + managed `WebFetch(domain:)` allow
    /// rules). When set, the derived `network.allowed_domains` is REPLACED by
    /// this list — user/project/local/flag domain allows are ignored. Denied
    /// domains still merge from all sources (handled in the rule walk). `None` ⇒
    /// no restriction (the merged allowlist is used as-is). The composition root
    /// computes this from the per-source settings it already loads.
    pub managed_allowed_domains: Option<Vec<String>>,
    /// `allowManagedReadPathsOnly` enforcement, the read-path twin of
    /// [`Self::managed_allowed_domains`]. `Some(paths)` ⇒ the flag is active in
    /// MANAGED settings; `filesystem.allow_read` is REPLACED by the MANAGED-source
    /// `sandbox.filesystem.allowRead` paths. `None` ⇒ no restriction.
    pub managed_read_paths: Option<Vec<String>>,
    /// `allowAppleEvents` SOURCE-RESTRICTED override. claude-code honors this
    /// setting ONLY from user, managed/policy, or CLI `--settings` (`flagSettings`)
    /// sources — project & local `.lingxi/settings*.json` are IGNORED
    /// (sandbox-adapter.ts 2.1.207 @223928133: `allowAppleEvents:[...managedSources,
    /// wr("flagSettings"), userSettings].map(z => z?.sandbox?.allowAppleEvents)
    /// .find(z => z !== undefined)` — first-defined wins over the RESTRICTED source
    /// list only; the general merge `e.sandbox?.X` is NOT used for this field).
    /// lingxi-core merges to a single `SettingsJson`, so the per-source resolution
    /// is done at the composition root and the result threaded here. `Some(v)` ⇒
    /// set `allow_apple_events = v`; `None` ⇒ leave the default (`false`), matching
    /// CC's `.find(...) === undefined ⇒ manager reads false`. The merged-blob
    /// `sandbox.allowAppleEvents` is intentionally NOT applied in
    /// [`convert_settings_to_runtime_config`] so project/local can never set it.
    pub allow_apple_events_override: Option<bool>,
    /// SOURCE-RESTRICTED `sandbox.network.strictAllowlist` (2.1.219), resolved
    /// per-source at the composition root exactly like
    /// [`Self::allow_apple_events_override`].
    ///
    /// The oracle honors it only from user / managed-policy / CLI `--settings`;
    /// project `.lingxi/settings.json` and `settings.local.json` are IGNORED.
    /// `None` ⇒ leave the default `false`.
    pub strict_allowlist_override: Option<bool>,
    /// SOURCE-RESTRICTED `sandbox.ripgrep` (2.1.232), resolved per-source at the
    /// composition root exactly like [`Self::allow_apple_events_override`].
    ///
    /// The oracle honors it only from user / managed-policy / CLI `--settings`;
    /// project `.lingxi/settings.json` and `settings.local.json` are IGNORED.
    /// `None` ⇒ leave the runtime default config.
    pub ripgrep_override: Option<RipgrepConfig>,
}

/// Parse a `Tool(content)` permission rule string into `(tool, content)`.
///
/// Returns `None` if the rule has no parentheses (a bare `Tool` rule covers
/// all calls and doesn't carry a filesystem path).
fn parse_rule(rule: &str) -> Option<(&str, &str)> {
    let open = rule.find('(')?;
    if !rule.ends_with(')') {
        return None;
    }
    let tool = &rule[..open];
    let content = &rule[open + 1..rule.len() - 1];
    Some((tool, content))
}

/// Convert claude-code `SettingsJson` into `SandboxRuntimeConfig`.
///
/// Walk `permissions.allow` and `permissions.deny`:
/// - `Edit(path)` allow  → `filesystem.allow_write`
/// - `Edit(path)` deny   → `filesystem.deny_write`
/// - `Read(path)` deny   → `filesystem.deny_read`
/// - `WebFetch(domain:host)` allow → `network.allowed_domains`
///
/// `additional_directories` (settings) is UNIONED with `ctx.additional_md_dirs`
/// (session) into `allow_write` with insertion-order dedup (TS `Set` semantics,
/// sandbox-adapter.ts:295-299).
///
/// Seeds (sandbox-adapter.ts:225-299, applied BEFORE the rule walk):
/// - `allow_write` starts with `["."]`, then `ctx.lingxi_temp_dir` if `Some`;
/// - `deny_write` is extended with `ctx.settings_file_paths`, then
///   `ctx.managed_drop_in_dir` if `Some`, then `ctx.cwd_settings_paths` and
///   `ctx.skills_dirs`;
/// - `ctx.worktree_main_repo_path` is pushed into `allow_write` if `Some`.
///
/// Bare-git paths (`HEAD`/`objects`/`refs`/`hooks`/`config`) are intentionally
/// NOT seeded here — they are owned by the posix `prepare` layer.
///
/// Any user-supplied `sandbox` subsection values override the derived defaults.
#[must_use]
pub fn convert_settings_to_runtime_config(
    settings: &SettingsJson,
    ctx: &SandboxConvertContext,
) -> SandboxRuntimeConfig {
    // allowManagedDomainsOnly / allowManagedReadPathsOnly are ENFORCED via the
    // managed-source subsets the composition root threads on
    // [`SandboxConvertContext::managed_allowed_domains`] /
    // [`SandboxConvertContext::managed_read_paths`]. claude-code resolves these
    // source-aware (`getSettingsForSource('policySettings')`,
    // sandbox-adapter.ts:181-210, 343-347): when managed settings set the flag,
    // lower-source `allowedDomains` / `allowRead` are dropped and only the
    // managed allowlist survives (denied domains still merge from all sources).
    // lingxi-core merges to a single `SettingsJson`, so the per-source decision
    // is made at the composition root and the resolved allowlist is applied as an
    // override at the end of this function (see the `managed_*` blocks below).
    let settings_dir: PathBuf = settings
        .settings_dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let mut filesystem = FilesystemRestrictionConfig::default();
    let mut network = NetworkRestrictionConfig::default();

    // --- Seeds (sandbox-adapter.ts:225-299), applied BEFORE the rule walk. ---
    // Always include current directory and Claude temp directory as writable.
    filesystem.allow_write.push(".".to_string());
    if let Some(tmp) = &ctx.lingxi_temp_dir {
        filesystem.allow_write.push(tmp.clone());
    }
    // Always deny writes to settings.json files / managed drop-in dir / cwd
    // settings / skills dirs to prevent sandbox escape.
    filesystem
        .deny_write
        .extend(ctx.settings_file_paths.iter().cloned());
    if let Some(dir) = &ctx.managed_drop_in_dir {
        filesystem.deny_write.push(dir.clone());
    }
    filesystem
        .deny_write
        .extend(ctx.cwd_settings_paths.iter().cloned());
    filesystem
        .deny_write
        .extend(ctx.skills_dirs.iter().cloned());
    if let Some(dir) = &ctx.task_output_dir {
        filesystem.deny_write.push(dir.clone());
    }
    // Git worktree main repo path needs write access for index.lock etc.
    if let Some(main_repo) = &ctx.worktree_main_repo_path {
        filesystem.allow_write.push(main_repo.clone());
    }

    if let Some(perms) = &settings.permissions {
        for rule_string in &perms.allow {
            apply_rule(
                rule_string,
                &settings_dir,
                &mut filesystem,
                &mut network,
                true,
            );
        }
        for rule_string in &perms.deny {
            apply_rule(
                rule_string,
                &settings_dir,
                &mut filesystem,
                &mut network,
                false,
            );
        }
    }

    // Additional directories: insertion-order-dedup union of
    // `permissions.additionalDirectories` (settings) and `ctx.additional_md_dirs`
    // (session). TS `new Set([...settings, ...session])` semantics
    // (sandbox-adapter.ts:295-299) — first occurrence wins, later dups dropped.
    {
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let settings_dirs = settings
            .permissions
            .as_ref()
            .map_or(&[][..], |p| p.additional_directories.as_slice());
        for dir in settings_dirs.iter().chain(ctx.additional_md_dirs.iter()) {
            if seen.insert(dir.as_str()) {
                filesystem.allow_write.push(dir.clone());
            }
        }
    }

    // Now apply any user-supplied `sandbox` subsection overrides.
    let mut cfg = SandboxRuntimeConfig {
        network,
        filesystem,
        ..Default::default()
    };

    if let Some(s) = &settings.sandbox {
        if let Some(v) = s.enabled {
            cfg.enabled = v;
        }
        if let Some(v) = s.fail_if_unavailable {
            cfg.fail_if_unavailable = v;
        }
        if let Some(v) = &s.enabled_platforms {
            cfg.enabled_platforms = Some(v.clone());
        }
        if let Some(v) = s.auto_allow_bash_if_sandboxed {
            cfg.auto_allow_bash_if_sandboxed = v;
        }
        if let Some(v) = s.allow_unsandboxed_commands {
            cfg.allow_unsandboxed_commands = v;
        }
        if let Some(v) = &s.network {
            // Merge: keep allowed_domains from WebFetch rules + values from
            // the sandbox.network.allowedDomains subsection. Likewise preserve
            // the DERIVED denied_domains (from WebFetch deny rules) and combine
            // with any sandbox.network.deniedDomains — mirror the allowed combine
            // so a user network override never clobbers derived deny entries.
            let mut merged = v.clone();
            merged.allowed_domains = {
                // claude-code order: [sandbox.network.allowedDomains ...,
                // WebFetch(domain:) allow ...] (sandbox-adapter.ts:198-209).
                // The subsection domains come FIRST, then the WebFetch-derived ones.
                let mut combined = v.allowed_domains.clone();
                combined.extend(cfg.network.allowed_domains.iter().cloned());
                combined
            };
            merged.denied_domains = {
                let mut combined = cfg.network.denied_domains.clone();
                combined.extend(v.denied_domains.iter().cloned());
                combined
            };
            cfg.network = merged;
        }
        if let Some(v) = &s.filesystem {
            // Merge: derived deny/allow paths + user-configured ones.
            let mut merged = v.clone();
            // sandbox.filesystem.* paths use STANDARD semantics, resolved via
            // resolveSandboxFilesystemPath (sandbox-adapter.ts:334-345, #30067) —
            // NOT the permission-rule `/path = settings-relative` convention.
            let resolve = |p: &str| resolve_sandbox_filesystem_path(p, &settings_dir);
            merged.allow_write = {
                let mut combined = cfg.filesystem.allow_write.clone();
                combined.extend(v.allow_write.iter().map(|p| resolve(p)));
                combined
            };
            merged.deny_write = {
                let mut combined = cfg.filesystem.deny_write.clone();
                combined.extend(v.deny_write.iter().map(|p| resolve(p)));
                combined
            };
            merged.deny_read = {
                let mut combined = cfg.filesystem.deny_read.clone();
                combined.extend(v.deny_read.iter().map(|p| resolve(p)));
                combined
            };
            merged.allow_read = {
                let mut combined = cfg.filesystem.allow_read.clone();
                combined.extend(v.allow_read.iter().map(|p| resolve(p)));
                combined
            };
            cfg.filesystem = merged;
        }
        if let Some(v) = &s.ignore_violations {
            cfg.ignore_violations.clone_from(v);
        }
        // `sandbox.credentials` (HP-6). Both `deny` and `mask` withhold the
        // credential here; see `sandbox::credentials` for why `mask` degrades
        // rather than falling through to no protection. Paths are resolved the
        // same way every other filesystem entry is, so `~` and a relative
        // spelling reach the same file the user meant.
        if let Some(v) = &s.credentials {
            let resolved = crate::credentials::resolve(v);
            cfg.filesystem.deny_read.extend(
                resolved
                    .deny_read_paths
                    .iter()
                    .map(|p| resolve_sandbox_filesystem_path(p, &settings_dir)),
            );
            for name in resolved.deny_env_vars {
                if !cfg.credential_deny_env.contains(&name) {
                    cfg.credential_deny_env.push(name);
                }
            }
            // ⚠️ A degraded `mask` changes what the user's commands can do, so
            // it must not be silent. This is the only channel this layer has —
            // the sandbox crate surfaces nothing user-facing today (its sibling
            // `linux_glob_pattern_warnings` has no production consumer either).
            for notice in &resolved.degraded {
                tracing::warn!("{notice}");
            }
        }
        if let Some(v) = s.enable_weaker_nested_sandbox {
            cfg.enable_weaker_nested_sandbox = v;
        }
        if let Some(v) = s.enable_weaker_network_isolation {
            cfg.enable_weaker_network_isolation = v;
        }
        // `allowPty` has NO settings key in claude-code (see
        // [`SandboxSettingsJson`]); `allowAppleEvents` is source-restricted and is
        // applied via `ctx.allow_apple_events_override` below (NOT from this merged
        // blob, so project/local settings can never set it — CC parity @223928133).
        if let Some(v) = &s.excluded_commands {
            cfg.excluded_commands.clone_from(v);
        }
    }

    // allowManagedDomainsOnly / allowManagedReadPathsOnly enforcement (applied
    // LAST so it overrides every merged source). When the composition root
    // detected the flag set in MANAGED settings, the derived allowlist is
    // REPLACED by the managed-source subset; denied domains / paths are left
    // untouched (they merge from all sources). See the field docs on
    // [`SandboxConvertContext`].
    if let Some(domains) = &ctx.managed_allowed_domains {
        cfg.network.allowed_domains = domains.clone();
    }
    if !cfg.filesystem.disabled {
        if let Some(read_paths) = &ctx.managed_read_paths {
            cfg.filesystem.allow_read = read_paths.clone();
        }
    }

    // allowAppleEvents SOURCE RESTRICTION (applied LAST). claude-code honors
    // `allowAppleEvents` only from user / managed-policy / CLI `--settings`
    // sources — project & local settings are ignored (sandbox-adapter.ts
    // @223928133). The composition root resolves the effective value per-source
    // (first-defined wins managed → flag → user) and threads it here. `None` ⇒
    // leave the default `false` (CC: `.find(...) === undefined` ⇒ manager reads
    // false). This deliberately supersedes any `sandbox.allowAppleEvents` in the
    // merged settings blob, so project/local can never enable Apple Events.
    if let Some(v) = ctx.allow_apple_events_override {
        cfg.allow_apple_events = v;
    }

    // strictAllowlist SOURCE RESTRICTION (applied LAST, same rationale as
    // allowAppleEvents). Honored only from user / managed-policy / CLI
    // `--settings`; a project or local tier that sets it is ignored, so the
    // merged-blob value is deliberately overwritten here rather than trusted.
    if let Some(v) = ctx.strict_allowlist_override {
        cfg.network.strict_allowlist = v;
    } else {
        // No honored source set it. Clear anything a non-honored tier merged
        // in — leaving it would let project settings enable the flag through
        // the merged blob, which is exactly what the restriction forbids.
        cfg.network.strict_allowlist = false;
    }

    // `sandbox.ripgrep` has the same source restriction as allowAppleEvents /
    // strictAllowlist in 2.1.232: only user / managed-policy / CLI `--settings`
    // may set it, so the merged-blob value is ignored and we apply the
    // composition-root override explicitly here. `None` clears any project/local
    // merged value back to the runtime default config.
    if let Some(v) = &ctx.ripgrep_override {
        cfg.ripgrep = v.clone();
    } else {
        cfg.ripgrep = RipgrepConfig::default();
    }

    cfg
}

/// Compute the MANAGED-source domain allowlist for `allowManagedDomainsOnly`
/// enforcement: the managed `sandbox.network.allowedDomains` plus every
/// `WebFetch(domain:host)` allow rule in the managed `permissions.allow`. The
/// composition root calls this on the parsed MANAGED settings (only) and threads
/// the result onto [`SandboxConvertContext::managed_allowed_domains`] when the
/// managed `sandbox.network.allowManagedDomainsOnly` flag is `true`. Order mirrors
/// the normal derivation: subsection domains first, then WebFetch-derived ones.
#[must_use]
pub fn managed_domain_allowlist(managed: &SettingsJson) -> Vec<String> {
    let mut domains: Vec<String> = managed
        .sandbox
        .as_ref()
        .and_then(|s| s.network.as_ref())
        .map(|n| n.allowed_domains.clone())
        .unwrap_or_default();
    if let Some(perms) = &managed.permissions {
        for rule in &perms.allow {
            if let Some((tool, content)) = parse_rule(rule) {
                if tool == TOOL_WEBFETCH {
                    if let Some(domain) = content.strip_prefix("domain:") {
                        domains.push(domain.to_string());
                    }
                }
            }
        }
    }
    domains
}

/// Compute the MANAGED-source read-path allowlist for `allowManagedReadPathsOnly`
/// enforcement: the managed `sandbox.filesystem.allowRead`, resolved with the
/// same `resolveSandboxFilesystemPath` semantics the normal walk uses. The
/// composition root threads the result onto
/// [`SandboxConvertContext::managed_read_paths`] when the managed
/// `sandbox.filesystem.allowManagedReadPathsOnly` flag is `true`.
#[must_use]
pub fn managed_read_path_allowlist(managed: &SettingsJson, settings_dir: &Path) -> Vec<String> {
    managed
        .sandbox
        .as_ref()
        .and_then(|s| s.filesystem.as_ref())
        .map(|f| {
            f.allow_read
                .iter()
                .map(|p| resolve_sandbox_filesystem_path(p, settings_dir))
                .collect()
        })
        .unwrap_or_default()
}

/// Port of `resolveSandboxFilesystemPath` (sandbox-adapter.ts:138-146 + the
/// #30067 fix). UNLIKE permission-rule paths (`resolve_path_pattern_for_sandbox`),
/// `sandbox.filesystem.*` uses STANDARD path semantics:
/// - `//path` → `/path` (legacy permission-rule escape, kept for compat);
/// - everything else → `expandPath(pattern, settings_dir)`:
///   - `~`        → `home_dir`
///   - `~/rest`   → `home_dir/rest`
///   - absolute   → as-is (NOT settings-relative — this is the #30067 fix)
///   - relative   → resolved against `settings_dir`
///   - empty/ws   → `settings_dir`
///
/// `home_dir` is injected (the `sandbox` crate carries no `dirs` dependency);
/// see [`resolve_sandbox_filesystem_path`] for the wrapper that reads `$HOME`.
#[must_use]
pub(crate) fn resolve_sandbox_filesystem_path_with(
    pattern: &str,
    settings_dir: &Path,
    home_dir: &str,
) -> String {
    // Legacy escape: //path → /path.
    if let Some(stripped) = pattern.strip_prefix("//") {
        return format!("/{stripped}");
    }
    // expandPath(pattern, settings_dir):
    let trimmed = pattern.trim();
    if trimmed.is_empty() {
        return settings_dir.to_string_lossy().into_owned();
    }
    if trimmed == "~" {
        return home_dir.to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        // expandPath: join(homedir(), slice(2)) — home + "/" + rest. Node `join`
        // lexically collapses `.`/`..` (path.ts:64), so we normalize when the
        // result is absolute (home_dir normally is).
        let joined = format!("{home_dir}/{rest}");
        if joined.starts_with('/') {
            return crate::path_pattern::lexically_normalize_absolute(&joined);
        }
        return joined;
    }
    let p = Path::new(trimmed);
    if p.is_absolute() {
        // expandPath returns absolute paths via Node `normalize()` (path.ts:80),
        // which lexically collapses `.`/`..` — NOT settings-relative, NOT
        // filesystem-canonicalized.
        return crate::path_pattern::lexically_normalize_absolute(trimmed);
    }
    // Relative → Node `resolve(settings_dir, pattern)` (path.ts:84), which
    // lexically collapses `.`/`..`. Normalize after the join when absolute.
    let joined = settings_dir.join(trimmed).to_string_lossy().into_owned();
    if joined.starts_with('/') {
        return crate::path_pattern::lexically_normalize_absolute(&joined);
    }
    joined
}

/// Wrapper reading the real home dir from `$HOME` (POSIX). `expandPath` uses
/// `homedir()`; on the posix sandbox target that is `$HOME`. Falls back to an
/// empty string when unset (matching `path_utils.rs`'s `unwrap_or_default`).
#[must_use]
fn resolve_sandbox_filesystem_path(pattern: &str, settings_dir: &Path) -> String {
    let home_dir = std::env::var("HOME").unwrap_or_default();
    resolve_sandbox_filesystem_path_with(pattern, settings_dir, &home_dir)
}

/// Pure core of [`resolve_deny_write_symlink`] — a 1:1 port of the deny-write
/// symlink resolver `SS` from `convertToSandboxRuntimeConfig`
/// (`claude-code/src/utils/sandbox/sandbox-adapter.ts`, the 2.1.210 hardening).
///
/// When a seeded `.lingxi/*` deny-write path is a symlink, the sandbox must deny
/// the symlink's REAL target — otherwise a repository- or session-planted
/// `.lingxi/settings.json → /etc/evil` redirect lets a write escape the deny
/// scope (the write follows the link to a path the sandbox never denied). So
/// this resolves the literal path to its target BEFORE it enters `deny_write`.
///
/// Deliberately UNLIKE [`crate::runtime_config`]'s allowlist normalization (and
/// `sandbox-runtime`'s `is_symlink_outside_boundary`): it follows symlinks that
/// point OUTSIDE the workspace boundary, because that out-of-boundary target is
/// exactly the escape destination that must be denied.
///
/// Mirrors CC's three branches:
/// - `readlink` fails ⇒ not a symlink ⇒ return `path` unchanged
///   (CC: `catch { return gRt.push(e), e }` — the literal is kept; the `gRt`
///   tracking exists only for CC's per-command reconcile pass, which has no
///   analog in lingxi's build-once-at-boot sandbox config, so it is omitted);
/// - `realpath` succeeds ⇒ symlink with a live target ⇒ return the canonical
///   target (CC: `realpathSync(e)`);
/// - `realpath` fails ⇒ broken symlink ⇒ manually follow the link chain up to 8
///   hops from the link text (CC: `resolve(dirname(e), t)` then the 8-iteration
///   `readlinkSync` loop).
///
/// `readlink` returns the link's target text (or `None` when `path` is not a
/// symlink / cannot be read); `realpath` returns the fully-canonicalized path
/// (or `None` when it does not exist). Both are injected so the resolver is
/// deterministic and unit-testable without touching the filesystem.
pub(crate) fn resolve_deny_write_symlink_with<R, P>(path: &str, readlink: R, realpath: P) -> String
where
    R: Fn(&str) -> Option<String>,
    P: Fn(&str) -> Option<String>,
{
    // Not a symlink: keep the literal path (CC: `catch { return ..., e }`).
    let Some(link_text) = readlink(path) else {
        return path.to_string();
    };
    // Symlink with a resolvable target: deny the canonical target
    // (CC: `realpathSync(e)`).
    if let Some(resolved) = realpath(path) {
        return resolved;
    }
    // Broken symlink: manually follow the link chain up to 8 hops
    // (CC: `let n = resolve(dirname(e), t); for (o=0; o<8; o++) { … }`).
    let mut current = resolve_link_target(path, &link_text);
    for _ in 0..8 {
        let Some(next) = readlink(&current) else {
            break;
        };
        current = resolve_link_target(&current, &next);
    }
    current
}

/// `path.resolve(path.dirname(from), link)`: an absolute `link` wins outright,
/// otherwise it is joined onto `from`'s parent directory; the result is
/// lexically normalized like Node's `path.resolve` (collapsing `.`/`..`).
fn resolve_link_target(from: &str, link: &str) -> String {
    let parent = Path::new(from)
        .parent()
        .map_or_else(|| PathBuf::from("/"), Path::to_path_buf);
    let joined = if Path::new(link).is_absolute() {
        link.to_string()
    } else {
        parent.join(link).to_string_lossy().into_owned()
    };
    if joined.starts_with('/') {
        crate::path_pattern::lexically_normalize_absolute(&joined)
    } else {
        joined
    }
}

/// Real-filesystem wrapper over [`resolve_deny_write_symlink_with`]: resolves a
/// seeded deny-write path to its symlink target using `std::fs::read_link`
/// (`readlinkSync`) and `std::fs::canonicalize` (`realpathSync`). Non-symlink
/// and non-existent seeds pass through unchanged, so only genuinely-symlinked
/// `.lingxi/*` paths are rewritten to their escape target before they enter the
/// sandbox `deny_write` list.
///
/// Applied by the composition root to each boot-seeded deny-write path so the
/// hardening fires against symlinks that exist at boot.
#[must_use]
pub fn resolve_deny_write_symlink(path: &str) -> String {
    resolve_deny_write_symlink_with(
        path,
        |p| {
            std::fs::read_link(p)
                .ok()
                .map(|t| t.to_string_lossy().into_owned())
        },
        |p| {
            std::fs::canonicalize(p)
                .ok()
                .map(|c| c.to_string_lossy().into_owned())
        },
    )
}

/// Re-resolve a live sandbox config's deny-write list so symlinks that appear
/// after boot are hardened before the next sandboxed command runs.
pub fn reconcile_deny_write_symlinks(cfg: &mut SandboxRuntimeConfig) {
    let mut seen = std::collections::HashSet::new();
    let mut reconciled = Vec::with_capacity(cfg.filesystem.deny_write.len());
    for path in &cfg.filesystem.deny_write {
        let resolved = resolve_deny_write_symlink(path);
        if seen.insert(resolved.clone()) {
            reconciled.push(resolved);
        }
    }
    cfg.filesystem.deny_write = reconciled;
}

fn apply_rule(
    rule_string: &str,
    settings_dir: &Path,
    filesystem: &mut FilesystemRestrictionConfig,
    network: &mut NetworkRestrictionConfig,
    is_allow: bool,
) {
    let Some((tool, content)) = parse_rule(rule_string) else {
        return;
    };
    match tool {
        TOOL_EDIT => {
            let resolved = resolve_path_pattern_for_sandbox(content, settings_dir);
            if is_allow {
                filesystem.allow_write.push(resolved);
            } else {
                filesystem.deny_write.push(resolved);
            }
        }
        TOOL_READ => {
            // claude-code extracts ONLY Read DENY rules → denyRead
            // (sandbox-adapter.ts:323-325). There is NO Read-allow → allowRead
            // branch; allowRead is sourced exclusively from
            // sandbox.filesystem.allowRead (sandbox-adapter.ts:343-347).
            if !is_allow {
                let resolved = resolve_path_pattern_for_sandbox(content, settings_dir);
                filesystem.deny_read.push(resolved);
            }
            // Read(allow) contributes nothing here.
        }
        TOOL_WEBFETCH => {
            if let Some(domain) = content.strip_prefix("domain:") {
                if is_allow {
                    network.allowed_domains.push(domain.to_string());
                } else {
                    // Deny rules feed the computed denylist, checked before the
                    // allowlist (sandbox-adapter.ts:212-220, 362).
                    network.denied_domains.push(domain.to_string());
                }
            }
        }
        _ => {
            // Bash, Task, etc. are not part of the sandbox filesystem map.
        }
    }
}

/// Port of `getLinuxGlobPatternWarnings` from
/// `claude-code/src/utils/sandbox/sandbox-adapter.ts`.
///
/// Returns the verbatim permission-rule strings whose path content contains
/// glob characters `* ? [ ]` (excluding a trailing `/**`). bubblewrap cannot
/// resolve globs, so claude-code surfaces these as user-facing warnings on
/// Linux/WSL.
///
/// Caller is expected to only invoke this on Linux/WSL hosts where sandbox is
/// enabled; this function does not check the platform itself (that's the
/// caller's responsibility — keeps this function purely string-driven).
#[must_use]
pub fn linux_glob_pattern_warnings(settings: &SettingsJson) -> Vec<String> {
    let Some(perms) = &settings.permissions else {
        return Vec::new();
    };
    let mut warnings = Vec::new();
    for rule_string in perms.allow.iter().chain(perms.deny.iter()) {
        let Some((tool, content)) = parse_rule(rule_string) else {
            continue;
        };
        if tool != TOOL_EDIT && tool != TOOL_READ {
            continue;
        }
        if has_globs_excluding_trailing_double_star(content) {
            warnings.push(rule_string.clone());
        }
    }
    warnings
}

/// `true` iff `path` contains `*`, `?`, `[`, or `]` anywhere outside a
/// trailing `/**`. Mirrors the JS regex `/[*?\[\]]/.test(stripped)` after
/// `path.replace(/\/\*\*$/, '')`.
fn has_globs_excluding_trailing_double_star(path: &str) -> bool {
    let stripped = path.strip_suffix("/**").unwrap_or(path);
    stripped.chars().any(|c| matches!(c, '*' | '?' | '[' | ']'))
}

#[cfg(test)]
mod apple_events_source_tests {
    use super::{convert_settings_to_runtime_config, SandboxConvertContext};
    use crate::runtime_config::{RipgrepConfig, SettingsJson};

    fn convert(json: &str, ctx: &SandboxConvertContext) -> bool {
        let settings: SettingsJson = serde_json::from_str(json).expect("settings parse");
        convert_settings_to_runtime_config(&settings, ctx).allow_apple_events
    }

    #[test]
    fn default_false_without_override() {
        assert!(!convert("{}", &SandboxConvertContext::default()));
    }

    #[test]
    fn merged_blob_does_not_set_apple_events() {
        // A `sandbox.allowAppleEvents: true` present in the MERGED settings blob
        // must NOT flip the flag — claude-code source-restricts allowAppleEvents
        // (@223928133), so it is applied ONLY via `ctx.allow_apple_events_override`.
        // This models a project/local settings tier trying to enable it: ignored.
        let json = r#"{"sandbox": {"allowAppleEvents": true}}"#;
        assert!(
            !convert(json, &SandboxConvertContext::default()),
            "merged-blob allowAppleEvents must be ignored (project/local can't set it)"
        );
    }

    #[test]
    fn context_override_true_enables() {
        let ctx = SandboxConvertContext {
            allow_apple_events_override: Some(true),
            ..Default::default()
        };
        assert!(convert("{}", &ctx));
        // Even with a merged blob explicitly false, the honored-source override wins.
        assert!(convert(r#"{"sandbox": {"allowAppleEvents": false}}"#, &ctx));
    }

    #[test]
    fn context_override_false_disables() {
        let ctx = SandboxConvertContext {
            allow_apple_events_override: Some(false),
            ..Default::default()
        };
        // Honored source set it to false: stays false, and a merged-blob `true`
        // (e.g. from an ignored project tier) cannot override it.
        assert!(!convert(r#"{"sandbox": {"allowAppleEvents": true}}"#, &ctx));
    }

    #[test]
    fn merged_blob_does_not_set_ripgrep() {
        let settings: SettingsJson =
            serde_json::from_str(r#"{"sandbox":{"ripgrep":{"command":"rg","args":["--hidden"]}}}"#)
                .expect("settings parse");
        let cfg = convert_settings_to_runtime_config(&settings, &SandboxConvertContext::default());
        assert_eq!(cfg.ripgrep.command, "");
        assert!(cfg.ripgrep.args.is_empty());
        assert_eq!(cfg.ripgrep.argv0, None);
    }

    #[test]
    fn context_override_applies_ripgrep() {
        let settings: SettingsJson =
            serde_json::from_str(r#"{"sandbox":{"ripgrep":{"command":"ignored"}}}"#)
                .expect("settings parse");
        let ctx = SandboxConvertContext {
            ripgrep_override: Some(RipgrepConfig {
                command: "/usr/bin/rg".to_string(),
                args: vec!["--no-config".to_string()],
                argv0: Some("rg".to_string()),
            }),
            ..Default::default()
        };
        let cfg = convert_settings_to_runtime_config(&settings, &ctx);
        assert_eq!(cfg.ripgrep.command, "/usr/bin/rg");
        assert_eq!(cfg.ripgrep.args, vec!["--no-config"]);
        assert_eq!(cfg.ripgrep.argv0.as_deref(), Some("rg"));
    }
}

#[cfg(test)]
mod resolve_fs_path_tests {
    use super::resolve_sandbox_filesystem_path_with;
    use std::path::Path;
    #[test]
    fn tilde_expands_to_injected_home() {
        let sd = Path::new("/home/u/.lingxi");
        assert_eq!(
            resolve_sandbox_filesystem_path_with("~", sd, "/home/u"),
            "/home/u"
        );
        assert_eq!(
            resolve_sandbox_filesystem_path_with("~/.cargo", sd, "/home/u"),
            "/home/u/.cargo"
        );
    }

    #[test]
    fn relative_dot_prefix_resolves_against_settings_dir() {
        // `./src` is relative → resolve(settings_dir, "./src") collapses `.`.
        let sd = Path::new("/proj/.lingxi");
        assert_eq!(
            resolve_sandbox_filesystem_path_with("./src", sd, "/home/u"),
            "/proj/.lingxi/src"
        );
    }

    #[test]
    fn absolute_collapses_dot_dot() {
        // Absolute → Node normalize(): `/a/../b` → `/b`.
        let sd = Path::new("/proj/.lingxi");
        assert_eq!(
            resolve_sandbox_filesystem_path_with("/a/../b", sd, "/home/u"),
            "/b"
        );
    }

    #[test]
    fn relative_dot_dot_collapses_after_join() {
        // Relative `x/../y` resolved under settings_dir → `<dir>/y`.
        let sd = Path::new("/proj/.lingxi");
        assert_eq!(
            resolve_sandbox_filesystem_path_with("x/../y", sd, "/home/u"),
            "/proj/.lingxi/y"
        );
    }

    #[test]
    fn tilde_rest_collapses_dot_dot() {
        // `~/a/../b` → join(home, "a/../b") collapses → `/home/u/b`.
        let sd = Path::new("/proj/.lingxi");
        assert_eq!(
            resolve_sandbox_filesystem_path_with("~/a/../b", sd, "/home/u"),
            "/home/u/b"
        );
    }
}

#[cfg(test)]
mod deny_write_symlink_tests {
    use super::{reconcile_deny_write_symlinks, resolve_deny_write_symlink_with};
    use crate::runtime_config::SandboxRuntimeConfig;
    use std::collections::HashMap;

    // Not a symlink (`readlink` returns None): the literal path is kept — the
    // common case, so non-symlink deny-write seeds are unchanged (CC: `catch {
    // return gRt.push(e), e }`).
    #[test]
    fn non_symlink_keeps_literal() {
        let out = resolve_deny_write_symlink_with(
            "/proj/.lingxi/settings.json",
            |_| None,
            |_| Some("/should/not/be/used".to_string()),
        );
        assert_eq!(out, "/proj/.lingxi/settings.json");
    }

    // Symlink with a live target: `realpath` wins and the CANONICAL target is
    // denied — even when it escapes the workspace boundary (CC: `realpathSync`).
    #[test]
    fn symlink_resolves_to_realpath_target() {
        let out = resolve_deny_write_symlink_with(
            "/proj/.lingxi/settings.json",
            |_| Some("/etc/evil".to_string()),
            |p| (p == "/proj/.lingxi/settings.json").then(|| "/etc/evil".to_string()),
        );
        assert_eq!(out, "/etc/evil");
    }

    // Broken symlink (`realpath` fails): manually resolve one hop from the link
    // text against the link's parent dir (CC: `resolve(dirname(e), t)`).
    #[test]
    fn broken_symlink_relative_target_resolves_against_parent() {
        let out = resolve_deny_write_symlink_with(
            "/proj/.lingxi/settings.json",
            |p| (p == "/proj/.lingxi/settings.json").then(|| "../../evil".to_string()),
            |_| None,
        );
        // dirname = /proj/.lingxi → join("../../evil") → /evil (collapsed).
        assert_eq!(out, "/evil");
    }

    // Broken symlink with an absolute target: the absolute link text wins.
    #[test]
    fn broken_symlink_absolute_target_used_directly() {
        let out = resolve_deny_write_symlink_with(
            "/proj/.lingxi/skills",
            |p| (p == "/proj/.lingxi/skills").then(|| "/tmp/planted".to_string()),
            |_| None,
        );
        assert_eq!(out, "/tmp/planted");
    }

    // Broken symlink chain: follow up to 8 hops through the link text
    // (CC: the `for (o=0; o<8; o++)` readlink loop).
    #[test]
    fn broken_symlink_chain_follows_multiple_hops() {
        let mut links: HashMap<&str, &str> = HashMap::new();
        links.insert("/a/link1", "/a/link2");
        links.insert("/a/link2", "/a/link3");
        links.insert("/a/link3", "/final/target");
        let out = resolve_deny_write_symlink_with(
            "/a/link1",
            move |p| links.get(p).map(|s| (*s).to_string()),
            |_| None, // realpath always fails ⇒ manual chain walk
        );
        assert_eq!(out, "/final/target");
    }

    // The manual chain is bounded at 8 hops: an infinite loop terminates on the
    // last resolved link rather than hanging (CC bounds the loop at 8).
    #[test]
    fn broken_symlink_chain_is_bounded() {
        let out = resolve_deny_write_symlink_with(
            "/a/loop",
            // Every path is a symlink to itself → never resolves, but bounded.
            |p| Some(p.to_string()),
            |_| None,
        );
        assert_eq!(out, "/a/loop");
    }

    #[test]
    fn reconcile_deny_write_symlinks_deduplicates_resolved_targets() {
        let mut cfg = SandboxRuntimeConfig::default();
        cfg.filesystem.deny_write = vec!["/a/loop".into(), "/a/loop".into(), "/b".into()];
        reconcile_deny_write_symlinks(&mut cfg);
        assert_eq!(cfg.filesystem.deny_write, vec!["/a/loop", "/b"]);
    }
}

#[cfg(test)]
mod deny_merge_tests {
    use super::{convert_settings_to_runtime_config, SandboxConvertContext};
    use crate::runtime_config::SettingsJson;
    use serde_json::json;

    /// PARITY 2.1.263: permission DENY rules feed the sandbox's filesystem deny
    /// lists — `Edit(path)` deny → `deny_write`, `Read(path)` deny →
    /// `deny_read` — and `sandbox.filesystem.denyWrite` / `denyRead` from the
    /// same settings merge into those same lists. The oracle documents the
    /// merge in the setting's own schema text ("Merged with paths from Read(...)
    /// deny permission rules.").
    ///
    /// The behaviour was already implemented; this pins it, because nothing did.
    #[test]
    fn permission_deny_rules_and_settings_deny_paths_merge() {
        let settings: SettingsJson = serde_json::from_value(json!({
            "permissions": {
                "allow": ["Edit(src/**)", "Read(docs/**)"],
                "deny": ["Edit(src/generated/**)", "Read(secret.txt)"]
            },
            "settingsDir": "/proj/.lingxi",
            "sandbox": {
                "enabled": true,
                "filesystem": {
                    "denyWrite": ["extra/no-write"],
                    "denyRead": ["extra/no-read"]
                }
            }
        }))
        .expect("settings parse");
        let cfg = convert_settings_to_runtime_config(&settings, &SandboxConvertContext::default());

        let has = |v: &[String], needle: &str| v.iter().any(|p| p.contains(needle));
        // From the permission rules.
        assert!(
            has(&cfg.filesystem.deny_write, "src/generated"),
            "an Edit deny rule must reach deny_write: {:?}",
            cfg.filesystem.deny_write
        );
        assert!(
            has(&cfg.filesystem.deny_read, "secret.txt"),
            "a Read deny rule must reach deny_read: {:?}",
            cfg.filesystem.deny_read
        );
        // …merged with the ones the settings file lists directly.
        assert!(
            has(&cfg.filesystem.deny_write, "extra/no-write"),
            "sandbox.filesystem.denyWrite must merge in: {:?}",
            cfg.filesystem.deny_write
        );
        assert!(
            has(&cfg.filesystem.deny_read, "extra/no-read"),
            "sandbox.filesystem.denyRead must merge in: {:?}",
            cfg.filesystem.deny_read
        );
        // The Edit ALLOW rule still reaches allow_write, so the deny walk did
        // not simply swallow every rule.
        assert!(
            has(&cfg.filesystem.allow_write, "src"),
            "an Edit allow rule must still reach allow_write: {:?}",
            cfg.filesystem.allow_write
        );
    }

    /// 🚨 The asymmetry the oracle documents at `apply_rule`: a `Read` ALLOW
    /// rule contributes NOTHING — `allow_read` comes only from
    /// `sandbox.filesystem.allowRead`. Easy to "fix" into symmetry and thereby
    /// widen sandbox reads.
    #[test]
    fn a_read_allow_rule_does_not_widen_allow_read() {
        let settings: SettingsJson = serde_json::from_value(json!({
            "permissions": { "allow": ["Read(/etc/**)"] },
            "settingsDir": "/proj/.lingxi",
            "sandbox": { "enabled": true }
        }))
        .expect("settings parse");
        let cfg = convert_settings_to_runtime_config(&settings, &SandboxConvertContext::default());
        assert!(
            !cfg.filesystem.allow_read.iter().any(|p| p.contains("etc")),
            "a Read allow rule must not reach allow_read: {:?}",
            cfg.filesystem.allow_read
        );
    }
}

#[cfg(test)]
mod filesystem_disabled_tests {
    use super::{convert_settings_to_runtime_config, SandboxConvertContext};
    use crate::runtime_config::{FilesystemRestrictionConfig, SandboxSettingsJson, SettingsJson};
    use serde_json::json;
    use std::path::PathBuf;

    fn settings_with_disabled_filesystem() -> SettingsJson {
        serde_json::from_value(json!({
            "permissions": {
                "allow": ["Edit(src/main.rs)"],
                "deny": ["Read(secret.txt)"]
            },
            "settingsDir": "/proj/.lingxi",
            "sandbox": {
                "enabled": true,
                "filesystem": {
                    "disabled": true,
                    "allowWrite": ["custom/write"],
                    "allowRead": ["custom/read"]
                }
            }
        }))
        .expect("settings parse")
    }

    #[test]
    fn merged_filesystem_disabled_flag_survives() {
        let cfg = convert_settings_to_runtime_config(
            &settings_with_disabled_filesystem(),
            &SandboxConvertContext::default(),
        );
        assert!(cfg.filesystem.disabled);
        assert!(cfg
            .filesystem
            .allow_write
            .contains(&"/proj/.lingxi/custom/write".to_string()));
        assert!(cfg
            .filesystem
            .allow_read
            .contains(&"/proj/.lingxi/custom/read".to_string()));
    }

    #[test]
    fn managed_read_override_is_ignored_when_filesystem_disabled() {
        let cfg = convert_settings_to_runtime_config(
            &settings_with_disabled_filesystem(),
            &SandboxConvertContext {
                managed_read_paths: Some(vec!["/managed/only".to_string()]),
                ..SandboxConvertContext::default()
            },
        );
        assert!(cfg.filesystem.disabled);
        assert!(cfg
            .filesystem
            .allow_read
            .contains(&"/proj/.lingxi/custom/read".to_string()));
        assert!(!cfg
            .filesystem
            .allow_read
            .contains(&"/managed/only".to_string()));
    }

    #[test]
    fn disabled_flag_deserializes_from_filesystem_subtree() {
        let filesystem: FilesystemRestrictionConfig =
            serde_json::from_value(json!({ "disabled": true })).expect("filesystem parse");
        assert!(filesystem.disabled);

        let settings = SettingsJson {
            settings_dir: Some(PathBuf::from("/proj/.lingxi")),
            sandbox: Some(SandboxSettingsJson {
                filesystem: Some(filesystem),
                ..Default::default()
            }),
            ..Default::default()
        };
        let cfg = convert_settings_to_runtime_config(&settings, &SandboxConvertContext::default());
        assert!(cfg.filesystem.disabled);
    }
}
