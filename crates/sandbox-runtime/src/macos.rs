//! The macOS Seatbelt (SBPL) sandbox backend. Ported 1:1 from
//! `@anthropic-ai/sandbox-runtime@0.0.54`.
//!
//! Reference of truth (source-line cites throughout):
//! `docs/superpowers/references/sandbox-runtime-0.0.54/dist/sandbox/macos-sandbox-utils.js`.
//!
//! SECURITY-RELEVANT: the generated SBPL profile *is* the macOS sandbox. The
//! static rule text is reproduced byte-for-byte from the reference (the
//! Chrome-derived process/mach/iokit/sysctl allowlists, the conditional
//! sections, the network/unix-socket rules) and the dynamic read/write rules are
//! built branch-for-branch from the [`ReadConfig`]/[`WriteConfig`] inputs so the
//! later-rule-wins Seatbelt semantics match exactly.
//!
//! The pure profile-text generation ([`generate_sandbox_profile`],
//! [`escape_path`], [`mac_get_mandatory_deny_patterns`], the rule generators) is
//! portable and unit-tested on every platform. The `sandbox-exec`-invoking glue
//! ([`wrap_command_with_sandbox_macos`]) and the `log stream` violation monitor
//! ([`start_macos_sandbox_log_monitor`]) are gated to `target_os = "macos"`.
//!
//! # Divergence from the TS (documented, intentional)
//!
//! - The TS module computes `sessionSuffix` once at module load
//!   (`macos-sandbox-utils.js:34`). This port computes it once via a process-wide
//!   [`std::sync::OnceLock`] seeded from `getrandom`, so it is equally stable for
//!   the process lifetime. Behavior (a per-process random `_<9 chars>_SBX`
//!   suffix) is identical.
//! - `escapePath` is `JSON.stringify(pathStr)` (`macos-sandbox-utils.js:518-520`);
//!   [`escape_path`] uses `serde_json::to_string`, which produces the identical
//!   JSON string-escaping (`"` → `\"`, `\` → `\\`, control chars → `\uXXXX`).
//! - [`generate_read_rules`] additionally emits a "re-deny a nested deny inside
//!   a re-allowed subpath" pass — the second `denyOnly` loop of the LATEST
//!   claude-code binary's `v0d` (v2.1.186). The pinned `0.0.54` reference does
//!   NOT have this pass; this is a deliberate SECURITY-tracking divergence toward
//!   the newer oracle. Without it a broad `allowWithinDeny` re-allow would win
//!   (Seatbelt is last-rule-wins) over a nested `denyOnly`, silently leaking read
//!   access to the nested deny. It re-denies only non-glob deny paths STRICTLY
//!   inside a re-allowed subpath, so it never over-denies.

use crate::env::{encode_sandboxed_command, Platform};
use crate::fs_args::{ReadConfig, WriteConfig};
use crate::path_utils::{
    contains_glob_chars, get_dangerous_directories, glob_to_regex, normalize_path_for_sandbox,
    DANGEROUS_FILES,
};

/// Per-process random session suffix `_<9 base36 chars>_SBX`, mirroring the TS
/// module-global `sessionSuffix` (`macos-sandbox-utils.js:34`).
///
/// The TS is `_${Math.random().toString(36).slice(2, 11)}_SBX` — 9 base36
/// characters. We draw randomness from `getrandom` and encode 9 base36 digits.
fn session_suffix() -> &'static str {
    use std::sync::OnceLock;
    static SUFFIX: OnceLock<String> = OnceLock::new();
    SUFFIX.get_or_init(|| {
        const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let mut bytes = [0u8; 9];
        // getrandom is best-effort; on the impossible failure path we fall back
        // to a fixed-but-still-valid suffix (the suffix only namespaces log
        // tags, it is not security-relevant).
        if getrandom::getrandom(&mut bytes).is_err() {
            return "_000000000_SBX".to_string();
        }
        let s: String = bytes
            .iter()
            .map(|b| ALPHABET[(*b as usize) % 36] as char)
            .collect();
        format!("_{s}_SBX")
    })
}

/// Generate a unique log tag for sandbox monitoring.
///
/// The command is base64-encoded (`encodeSandboxedCommand`) and wrapped as
/// `CMD64_<b64>_END_<sessionSuffix>`.
///
/// Ported from `macos-sandbox-utils.js:39-42` (`generateLogTag`).
#[must_use]
pub fn generate_log_tag(command: &str) -> String {
    let encoded = encode_sandboxed_command(command);
    format!("CMD64_{encoded}_END{}", session_suffix())
}

/// Escape a path for the sandbox profile using JSON string escaping.
///
/// The TS uses `JSON.stringify(pathStr)` (`macos-sandbox-utils.js:518-520`); the
/// result is a double-quoted, JSON-escaped string used inside
/// `(literal ...)`/`(regex ...)`/`(subpath ...)` forms. `serde_json::to_string`
/// of a `&str` produces the identical encoding.
///
/// Ported from `macos-sandbox-utils.js:515-520` (`escapePath`).
#[must_use]
pub fn escape_path(path_str: &str) -> String {
    // serde_json::to_string on a &str never fails.
    serde_json::to_string(path_str).unwrap_or_else(|_| format!("\"{path_str}\""))
}

/// POSIX `path.dirname` (local copy; the `path_utils` one is private).
fn posix_dirname(p: &str) -> String {
    let normalized = p.trim_end_matches('/');
    match normalized.rfind('/') {
        None => ".".to_string(),
        Some(0) => "/".to_string(),
        Some(idx) => normalized[..idx].to_string(),
    }
}

/// POSIX `path.resolve(cwd, p)` where `cwd` is absolute (local copy mirroring
/// `path_utils`'s private resolver — join then collapse `.`/`..`, no trailing
/// slash except root).
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

/// Get mandatory deny patterns as glob/absolute patterns (no filesystem
/// scanning — macOS Seatbelt does regex/glob matching directly).
///
/// `cwd` is taken as a parameter (the TS reads `process.cwd()`) so this stays
/// deterministic and testable. Produces, in order: for each dangerous file the
/// CWD-resolved absolute path then `**/<file>`; for each dangerous directory the
/// CWD-resolved absolute path then `**/<dir>/**`; `.git/hooks` (absolute) +
/// `**/.git/hooks/**`; and (unless `allow_git_config`) `.git/config` (absolute) +
/// `**/.git/config`. The list is deduped preserving first-seen order
/// (`[...new Set(...)]`).
///
/// Ported from `macos-sandbox-utils.js:11-33` (`macGetMandatoryDenyPatterns`).
#[must_use]
pub fn mac_get_mandatory_deny_patterns_with(allow_git_config: bool, cwd: &str) -> Vec<String> {
    let mut deny_paths: Vec<String> = Vec::new();

    // Dangerous files — static CWD path + glob for subtree.
    for file_name in DANGEROUS_FILES {
        deny_paths.push(posix_resolve(cwd, file_name));
        deny_paths.push(format!("**/{file_name}"));
    }
    // Dangerous directories.
    for dir_name in get_dangerous_directories() {
        deny_paths.push(posix_resolve(cwd, &dir_name));
        deny_paths.push(format!("**/{dir_name}/**"));
    }
    // Git hooks are always blocked.
    deny_paths.push(posix_resolve(cwd, ".git/hooks"));
    deny_paths.push("**/.git/hooks/**".to_string());
    // Git config — conditional.
    if !allow_git_config {
        deny_paths.push(posix_resolve(cwd, ".git/config"));
        deny_paths.push("**/.git/config".to_string());
    }

    // Dedup preserving order ([...new Set(...)]).
    let mut seen = std::collections::HashSet::new();
    deny_paths.retain(|p| seen.insert(p.clone()));
    deny_paths
}

/// Thin wrapper over [`mac_get_mandatory_deny_patterns_with`] reading the real
/// `std::env::current_dir()`.
///
/// Ported from `macos-sandbox-utils.js:11-33` (`macGetMandatoryDenyPatterns`).
#[must_use]
pub fn mac_get_mandatory_deny_patterns(allow_git_config: bool) -> Vec<String> {
    let cwd = std::env::current_dir()
        .map_or_else(|_| "/".to_string(), |p| p.to_string_lossy().into_owned());
    mac_get_mandatory_deny_patterns_with(allow_git_config, &cwd)
}

/// Get all ancestor directories for a path, up to (but not including) root.
///
/// Example: `/private/tmp/test/file.txt` ->
/// `["/private/tmp/test", "/private/tmp", "/private"]`.
///
/// Ported from `macos-sandbox-utils.js:47-61` (`getAncestorDirectories`).
fn get_ancestor_directories(path_str: &str) -> Vec<String> {
    let mut ancestors: Vec<String> = Vec::new();
    let mut current_path = posix_dirname(path_str);
    while current_path != "/" && current_path != "." {
        ancestors.push(current_path.clone());
        let parent_path = posix_dirname(&current_path);
        if parent_path == current_path {
            break;
        }
        current_path = parent_path;
    }
    ancestors
}

/// Split a normalized path at the first glob character (`*`, `?`, `[`, `]`) and
/// return the static prefix preceding it (TS `split(/[*?[\]]/)[0]`).
fn static_prefix(s: &str) -> String {
    match s.find(['*', '?', '[', ']']) {
        Some(idx) => s[..idx].to_string(),
        None => s.to_string(),
    }
}

/// Generate deny rules for file movement (`file-write-unlink`) and creation
/// (`file-write-create`) to protect paths from being bypassed via `mv`/rename or
/// symlink replacement.
///
/// Ported from `macos-sandbox-utils.js:73-120` (`generateMoveBlockingRules`).
fn generate_move_blocking_rules(path_patterns: &[String], log_tag: &str) -> Vec<String> {
    let mut rules: Vec<String> = Vec::new();
    let ops = ["file-write-unlink", "file-write-create"];

    for path_pattern in path_patterns {
        let normalized_path = normalize_path_for_sandbox(path_pattern);
        if contains_glob_chars(&normalized_path) {
            // Regex matching for glob patterns.
            let regex_pattern = glob_to_regex(&normalized_path);
            for op in ops {
                rules.push(format!("(deny {op}"));
                rules.push(format!("  (regex {})", escape_path(&regex_pattern)));
                rules.push(format!("  (with message \"{log_tag}\"))"));
            }
            // Extract the static prefix and block ancestor moves.
            let static_prefix = static_prefix(&normalized_path);
            if !static_prefix.is_empty() && static_prefix != "/" {
                let base_dir = if static_prefix.ends_with('/') {
                    static_prefix[..static_prefix.len() - 1].to_string()
                } else {
                    posix_dirname(&static_prefix)
                };
                // Block moves of the base directory itself.
                for op in ops {
                    rules.push(format!("(deny {op}"));
                    rules.push(format!("  (literal {})", escape_path(&base_dir)));
                    rules.push(format!("  (with message \"{log_tag}\"))"));
                }
                // Block moves of ancestor directories.
                for ancestor_dir in get_ancestor_directories(&base_dir) {
                    for op in ops {
                        rules.push(format!("(deny {op}"));
                        rules.push(format!("  (literal {})", escape_path(&ancestor_dir)));
                        rules.push(format!("  (with message \"{log_tag}\"))"));
                    }
                }
            }
        } else {
            // Subpath matching for literal paths.
            for op in ops {
                rules.push(format!("(deny {op}"));
                rules.push(format!("  (subpath {})", escape_path(&normalized_path)));
                rules.push(format!("  (with message \"{log_tag}\"))"));
            }
            // Block moves of ancestor directories.
            for ancestor_dir in get_ancestor_directories(&normalized_path) {
                for op in ops {
                    rules.push(format!("(deny {op}"));
                    rules.push(format!("  (literal {})", escape_path(&ancestor_dir)));
                    rules.push(format!("  (with message \"{log_tag}\"))"));
                }
            }
        }
    }
    rules
}

/// Generate filesystem read rules for the sandbox profile.
///
/// Two layers: `denyOnly` (deny reads from broad regions) then `allowWithinDeny`
/// (re-allow specific subpaths). In Seatbelt later rules win, so we emit
/// `(allow file-read*)` (default), then the denies, then the re-allows, then the
/// directory metadata + move-blocking rules + write-allow re-allows.
///
/// `write_allow_paths` are the write-allowed paths; their `file-write-unlink` /
/// `file-write-create` are re-allowed after the move-blocking denies (a specific
/// op-deny is not overridden by a later `(allow file-write*)` wildcard).
///
/// Ported from `macos-sandbox-utils.js:134-214` (`generateReadRules`).
fn generate_read_rules(
    config: Option<&ReadConfig>,
    log_tag: &str,
    write_allow_paths: Option<&[String]>,
) -> Vec<String> {
    let Some(config) = config else {
        return vec!["(allow file-read*)".to_string()];
    };

    let mut rules: Vec<String> = Vec::new();
    let mut denies_root = false;

    // Start by allowing everything.
    rules.push("(allow file-read*)".to_string());

    // Then deny specific paths.
    for path_pattern in &config.deny_only {
        let normalized_path = normalize_path_for_sandbox(path_pattern);
        if normalized_path == "/" {
            denies_root = true;
        }
        if contains_glob_chars(&normalized_path) {
            let regex_pattern = glob_to_regex(&normalized_path);
            rules.push("(deny file-read*".to_string());
            rules.push(format!("  (regex {})", escape_path(&regex_pattern)));
            rules.push(format!("  (with message \"{log_tag}\"))"));
        } else {
            rules.push("(deny file-read*".to_string());
            rules.push(format!("  (subpath {})", escape_path(&normalized_path)));
            rules.push(format!("  (with message \"{log_tag}\"))"));
        }
    }

    // Re-allow the literal root so path traversal works after (subpath "/").
    if denies_root {
        rules.push("(allow file-read* (literal \"/\"))".to_string());
    }

    // Re-allow specific paths within denied regions, tracking the non-glob
    // re-allowed subpaths so nested denies inside them can be re-applied below
    // (v0d's `s` set).
    let mut reallowed_subpaths: Vec<String> = Vec::new();
    for path_pattern in &config.allow_within_deny {
        let normalized_path = normalize_path_for_sandbox(path_pattern);
        if contains_glob_chars(&normalized_path) {
            let regex_pattern = glob_to_regex(&normalized_path);
            rules.push("(allow file-read*".to_string());
            rules.push(format!("  (regex {})", escape_path(&regex_pattern)));
            rules.push(format!("  (with message \"{log_tag}\"))"));
        } else {
            reallowed_subpaths.push(normalized_path.clone());
            rules.push("(allow file-read*".to_string());
            rules.push(format!("  (subpath {})", escape_path(&normalized_path)));
            rules.push(format!("  (with message \"{log_tag}\"))"));
        }
    }

    // Re-deny nested deny paths that fall INSIDE a re-allowed subpath (v0d's
    // second `denyOnly` pass): a broader re-allow above would otherwise win
    // (Seatbelt is last-rule-wins), silently leaking read access to a nested
    // deny. Only non-glob deny paths strictly within a re-allowed subpath are
    // re-emitted.
    for path_pattern in &config.deny_only {
        if contains_glob_chars(path_pattern) {
            continue;
        }
        let normalized_path = normalize_path_for_sandbox(path_pattern);
        if reallowed_subpaths
            .iter()
            .any(|allowed| normalized_path.starts_with(&format!("{allowed}/")))
        {
            rules.push("(deny file-read*".to_string());
            rules.push(format!("  (subpath {})", escape_path(&normalized_path)));
            rules.push(format!("  (with message \"{log_tag}\"))"));
        }
    }

    // Allow stat/lstat on all directories so realpath() can traverse.
    if !config.deny_only.is_empty() {
        rules.push("(allow file-read-metadata".to_string());
        rules.push("  (vnode-type DIRECTORY))".to_string());
    }

    // Block file movement to prevent bypass via mv/rename.
    rules.extend(generate_move_blocking_rules(&config.deny_only, log_tag));

    // Re-allow file-write-unlink / file-write-create for write-allowed paths.
    if let Some(write_allow_paths) = write_allow_paths {
        if !write_allow_paths.is_empty() {
            for path_pattern in write_allow_paths {
                let normalized_path = normalize_path_for_sandbox(path_pattern);
                for op in ["file-write-unlink", "file-write-create"] {
                    if contains_glob_chars(&normalized_path) {
                        let regex_pattern = glob_to_regex(&normalized_path);
                        rules.push(format!("(allow {op}"));
                        rules.push(format!("  (regex {})", escape_path(&regex_pattern)));
                        rules.push(format!("  (with message \"{log_tag}\"))"));
                    } else {
                        rules.push(format!("(allow {op}"));
                        rules.push(format!("  (subpath {})", escape_path(&normalized_path)));
                        rules.push(format!("  (with message \"{log_tag}\"))"));
                    }
                }
            }
        }
    }

    rules
}

/// Generate filesystem write rules for the sandbox profile.
///
/// `None` config => `(allow file-write*)` (no restrictions). Otherwise emit the
/// `allowOnly` allows, then the `denyWithinAllow` + mandatory deny patterns, then
/// the move-blocking rules over those deny paths.
///
/// Ported from `macos-sandbox-utils.js:218-256` (`generateWriteRules`).
fn generate_write_rules(
    config: Option<&WriteConfig>,
    log_tag: &str,
    allow_git_config: bool,
) -> Vec<String> {
    let Some(config) = config else {
        return vec!["(allow file-write*)".to_string()];
    };

    let mut rules: Vec<String> = Vec::new();

    // Allow rules.
    for path_pattern in &config.allow_only {
        let normalized_path = normalize_path_for_sandbox(path_pattern);
        if contains_glob_chars(&normalized_path) {
            let regex_pattern = glob_to_regex(&normalized_path);
            rules.push("(allow file-write*".to_string());
            rules.push(format!("  (regex {})", escape_path(&regex_pattern)));
            rules.push(format!("  (with message \"{log_tag}\"))"));
        } else {
            rules.push("(allow file-write*".to_string());
            rules.push(format!("  (subpath {})", escape_path(&normalized_path)));
            rules.push(format!("  (with message \"{log_tag}\"))"));
        }
    }

    // Combine user-specified + mandatory deny patterns.
    let mut deny_paths: Vec<String> = config.deny_within_allow.clone();
    deny_paths.extend(mac_get_mandatory_deny_patterns(allow_git_config));

    for path_pattern in &deny_paths {
        let normalized_path = normalize_path_for_sandbox(path_pattern);
        if contains_glob_chars(&normalized_path) {
            let regex_pattern = glob_to_regex(&normalized_path);
            rules.push("(deny file-write*".to_string());
            rules.push(format!("  (regex {})", escape_path(&regex_pattern)));
            rules.push(format!("  (with message \"{log_tag}\"))"));
        } else {
            rules.push("(deny file-write*".to_string());
            rules.push(format!("  (subpath {})", escape_path(&normalized_path)));
            rules.push(format!("  (with message \"{log_tag}\"))"));
        }
    }

    // Block file movement to prevent bypass via mv/rename.
    rules.extend(generate_move_blocking_rules(&deny_paths, log_tag));

    rules
}

/// Parameters for [`generate_sandbox_profile`] — a 1:1 mirror of the TS
/// `generateSandboxProfile` `params` object
/// (`macos-sandbox-utils.js:260`).
//
// The boolean flags are a faithful 1:1 transcription of the TS params object;
// collapsing them into enums would diverge from the reference shape.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default)]
pub struct ProfileParams<'a> {
    /// Read-restriction config (`denyOnly`). `None` => `(allow file-read*)`.
    pub read_config: Option<&'a ReadConfig>,
    /// Write-restriction config (`allowOnly`). `None` => `(allow file-write*)`.
    pub write_config: Option<&'a WriteConfig>,
    /// HTTP proxy port — emits `localhost:<port>` bind/inbound/outbound allows.
    pub http_proxy_port: Option<u16>,
    /// SOCKS proxy port — emits `localhost:<port>` bind/inbound/outbound allows.
    pub socks_proxy_port: Option<u16>,
    /// Whether to restrict the network (false => `(allow network*)`).
    pub needs_network_restriction: bool,
    /// Specific Unix-socket subpaths to allow (when not allowing all).
    pub allow_unix_sockets: Option<&'a [String]>,
    /// Allow all Unix sockets (`AF_UNIX` + path-regex `^/`).
    pub allow_all_unix_sockets: bool,
    /// Allow local-IP binding (`network-bind`/`-inbound`/`-outbound` on `*:*`).
    pub allow_local_binding: bool,
    /// User-specified XPC/Mach service names (trailing `*` => `global-name-prefix`).
    pub allow_mach_lookup: Option<&'a [String]>,
    /// Allow pseudo-terminal (pty) support.
    pub allow_pty: bool,
    /// Allow git config in the sandbox (passed to write rules).
    pub allow_git_config: bool,
    /// Enable weaker network isolation (trustd.agent mach-lookup).
    pub enable_weaker_network_isolation: bool,
    /// Allow Apple Events (appleeventsd / Launch Services open).
    pub allow_apple_events: bool,
    /// The log tag stamped into `(deny default ...)` and every rule message.
    pub log_tag: &'a str,
}

/// Generate the complete SBPL sandbox profile text.
///
/// Reproduces the verbatim static header (version 1, deny-default with logTag,
/// the Chrome-derived process/mach-lookup/iokit/system-socket/sysctl allowlists,
/// the conditional weaker-network/apple-events/user-mach-lookup/pty sections),
/// then the network section, then the read + write rules. Faithful to the exact
/// rule strings, global-names and sysctl names.
///
/// Ported from `macos-sandbox-utils.js:260-514` (`generateSandboxProfile`).
// A faithful 1:1 port of one large TS function; splitting it would obscure the
// byte-for-byte parity with the reference template.
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn generate_sandbox_profile(params: &ProfileParams<'_>) -> String {
    let log_tag = params.log_tag;
    let mut profile: Vec<String> = Vec::new();

    // ===== Verbatim static header (macos-sandbox-utils.js:261-431) =====
    profile.push("(version 1)".to_string());
    profile.push(format!("(deny default (with message \"{log_tag}\"))"));
    profile.push(String::new());
    profile.push(format!("; LogTag: {log_tag}"));
    profile.push(String::new());
    profile.push("; Essential permissions - based on Chrome sandbox policy".to_string());
    profile.push("; Process permissions".to_string());
    profile.push("(allow process-exec)".to_string());
    profile.push("(allow process-fork)".to_string());
    profile.push("(allow process-info* (target same-sandbox))".to_string());
    profile.push("(allow signal (target same-sandbox))".to_string());
    profile.push("(allow mach-priv-task-port (target same-sandbox))".to_string());
    profile.push(String::new());
    profile.push("; User preferences".to_string());
    profile.push("(allow user-preference-read)".to_string());
    profile.push(String::new());
    profile.push("; Mach IPC - specific services only (no wildcard)".to_string());
    profile.push("(allow mach-lookup".to_string());
    profile.push("  (global-name \"com.apple.audio.systemsoundserver\")".to_string());
    profile.push("  (global-name \"com.apple.distributed_notifications@Uv3\")".to_string());
    profile.push("  (global-name \"com.apple.FontObjectsServer\")".to_string());
    profile.push("  (global-name \"com.apple.fonts\")".to_string());
    profile.push("  (global-name \"com.apple.logd\")".to_string());
    profile.push("  (global-name \"com.apple.lsd.mapdb\")".to_string());
    profile.push("  (global-name \"com.apple.PowerManagement.control\")".to_string());
    profile.push("  (global-name \"com.apple.system.logger\")".to_string());
    profile.push("  (global-name \"com.apple.system.notification_center\")".to_string());
    profile.push("  (global-name \"com.apple.system.opendirectoryd.libinfo\")".to_string());
    profile.push("  (global-name \"com.apple.system.opendirectoryd.membership\")".to_string());
    profile.push("  (global-name \"com.apple.bsd.dirhelper\")".to_string());
    profile.push("  (global-name \"com.apple.securityd.xpc\")".to_string());
    profile.push("  (global-name \"com.apple.coreservices.launchservicesd\")".to_string());
    profile.push(")".to_string());
    profile.push(String::new());

    // Conditional: weaker network isolation (trustd.agent).
    if params.enable_weaker_network_isolation {
        profile.push(
            "; trustd.agent - needed for Go TLS certificate verification (weaker network isolation)"
                .to_string(),
        );
        profile.push("(allow mach-lookup (global-name \"com.apple.trustd.agent\"))".to_string());
    }

    // Conditional: Apple Events.
    if params.allow_apple_events {
        profile.push(
            "; Apple Events - opt-in; needed for open/osascript to talk to other apps (appleeventsd)"
                .to_string(),
        );
        profile.push("(allow appleevent-send)".to_string());
        profile.push(
            "(allow mach-lookup (global-name \"com.apple.coreservices.appleevents\"))".to_string(),
        );
        profile
            .push("; Launch Services open requests need the lsopen operation plus, on".to_string());
        profile
            .push("; macOS 14/15, coreservicesd and the quarantine resolver - without".to_string());
        profile.push("; these open fails with -10822 kLSServerCommunicationErr or -54".to_string());
        profile.push("(allow lsopen)".to_string());
        profile.push(
            "(allow mach-lookup (global-name \"com.apple.CoreServices.coreservicesd\"))"
                .to_string(),
        );
        profile.push(
            "(allow mach-lookup (global-name \"com.apple.coreservices.quarantine-resolver\"))"
                .to_string(),
        );
    }

    // Conditional: user-specified XPC/Mach services.
    if let Some(names) = params.allow_mach_lookup {
        if !names.is_empty() {
            profile.push("; User-specified XPC/Mach services".to_string());
            for name in names {
                if let Some(prefix) = name.strip_suffix('*') {
                    profile.push(format!(
                        "(allow mach-lookup (global-name-prefix {}))",
                        escape_path(prefix)
                    ));
                } else {
                    profile.push(format!(
                        "(allow mach-lookup (global-name {}))",
                        escape_path(name)
                    ));
                }
            }
        }
    }

    profile.push(String::new());
    profile.push("; POSIX IPC - shared memory".to_string());
    profile.push("(allow ipc-posix-shm)".to_string());
    profile.push(String::new());
    profile.push("; POSIX IPC - semaphores for Python multiprocessing".to_string());
    profile.push("(allow ipc-posix-sem)".to_string());
    profile.push(String::new());
    profile.push("; IOKit - specific operations only".to_string());
    profile.push("(allow iokit-open".to_string());
    profile.push("  (iokit-registry-entry-class \"IOSurfaceRootUserClient\")".to_string());
    profile.push("  (iokit-registry-entry-class \"RootDomainUserClient\")".to_string());
    profile.push("  (iokit-user-client-class \"IOSurfaceSendRight\")".to_string());
    profile.push(")".to_string());
    profile.push(String::new());
    profile.push("; IOKit properties".to_string());
    profile.push("(allow iokit-get-properties)".to_string());
    profile.push(String::new());
    profile.push("; Specific safe system-sockets, doesn't allow network access".to_string());
    profile.push(
        "(allow system-socket (require-all (socket-domain AF_SYSTEM) (socket-protocol 2)))"
            .to_string(),
    );
    profile.push(String::new());
    profile.push("; sysctl - specific sysctls only".to_string());
    profile.push("(allow sysctl-read".to_string());
    for name in SYSCTL_READ_NAMES {
        profile.push(format!("  (sysctl-name \"{name}\")"));
    }
    for prefix in SYSCTL_READ_PREFIXES {
        profile.push(format!("  (sysctl-name-prefix \"{prefix}\")"));
    }
    profile.push(")".to_string());
    profile.push(String::new());
    profile.push("; V8 thread calculations".to_string());
    profile.push("(allow sysctl-write".to_string());
    profile.push("  (sysctl-name \"kern.tcsm_enable\")".to_string());
    profile.push(")".to_string());
    profile.push(String::new());
    profile.push("; Distributed notifications".to_string());
    profile.push("(allow distributed-notification-post)".to_string());
    profile.push(String::new());
    profile.push("; Specific mach-lookup permissions for security operations".to_string());
    profile.push("(allow mach-lookup (global-name \"com.apple.SecurityServer\"))".to_string());
    profile.push(String::new());
    profile.push("; File I/O on device files".to_string());
    profile.push("(allow file-ioctl (literal \"/dev/null\"))".to_string());
    profile.push("(allow file-ioctl (literal \"/dev/zero\"))".to_string());
    profile.push("(allow file-ioctl (literal \"/dev/random\"))".to_string());
    profile.push("(allow file-ioctl (literal \"/dev/urandom\"))".to_string());
    profile.push("(allow file-ioctl (literal \"/dev/dtracehelper\"))".to_string());
    profile.push("(allow file-ioctl (literal \"/dev/tty\"))".to_string());
    profile.push(String::new());
    profile.push("(allow file-ioctl file-read-data file-write-data".to_string());
    profile.push("  (require-all".to_string());
    profile.push("    (literal \"/dev/null\")".to_string());
    profile.push("    (vnode-type CHARACTER-DEVICE)".to_string());
    profile.push("  )".to_string());
    profile.push(")".to_string());
    profile.push(String::new());

    // ===== Network section (macos-sandbox-utils.js:432-488) =====
    profile.push("; Network".to_string());
    if params.needs_network_restriction {
        // Allow local binding if requested.
        if params.allow_local_binding {
            profile.push("(allow network-bind (local ip \"*:*\"))".to_string());
            profile.push("(allow network-inbound (local ip \"*:*\"))".to_string());
            profile.push("(allow network-outbound (local ip \"*:*\"))".to_string());
        }

        // Unix domain sockets.
        if params.allow_all_unix_sockets {
            profile.push("(allow system-socket (socket-domain AF_UNIX))".to_string());
            profile
                .push("(allow network-bind (local unix-socket (path-regex #\"^/\")))".to_string());
            profile.push(
                "(allow network-outbound (remote unix-socket (path-regex #\"^/\")))".to_string(),
            );
        } else if let Some(sockets) = params.allow_unix_sockets {
            if !sockets.is_empty() {
                profile.push("(allow system-socket (socket-domain AF_UNIX))".to_string());
                for socket_path in sockets {
                    let normalized_path = normalize_path_for_sandbox(socket_path);
                    profile.push(format!(
                        "(allow network-bind (local unix-socket (subpath {})))",
                        escape_path(&normalized_path)
                    ));
                    profile.push(format!(
                        "(allow network-outbound (remote unix-socket (subpath {})))",
                        escape_path(&normalized_path)
                    ));
                }
            }
        }

        // Localhost TCP for the HTTP proxy.
        if let Some(port) = params.http_proxy_port {
            profile.push(format!(
                "(allow network-bind (local ip \"localhost:{port}\"))"
            ));
            profile.push(format!(
                "(allow network-inbound (local ip \"localhost:{port}\"))"
            ));
            profile.push(format!(
                "(allow network-outbound (remote ip \"localhost:{port}\"))"
            ));
        }
        // Localhost TCP for the SOCKS proxy.
        if let Some(port) = params.socks_proxy_port {
            profile.push(format!(
                "(allow network-bind (local ip \"localhost:{port}\"))"
            ));
            profile.push(format!(
                "(allow network-inbound (local ip \"localhost:{port}\"))"
            ));
            profile.push(format!(
                "(allow network-outbound (remote ip \"localhost:{port}\"))"
            ));
        }
    } else {
        profile.push("(allow network*)".to_string());
    }
    profile.push(String::new());

    // ===== Read rules (macos-sandbox-utils.js:489-495) =====
    let write_allow_paths = params.write_config.map(|c| c.allow_only.as_slice());
    profile.push("; File read".to_string());
    profile.extend(generate_read_rules(
        params.read_config,
        log_tag,
        write_allow_paths,
    ));
    profile.push(String::new());

    // ===== Write rules (macos-sandbox-utils.js:496-498) =====
    profile.push("; File write".to_string());
    profile.extend(generate_write_rules(
        params.write_config,
        log_tag,
        params.allow_git_config,
    ));

    // ===== Pty support (macos-sandbox-utils.js:499-512) =====
    if params.allow_pty {
        profile.push(String::new());
        profile.push("; Pseudo-terminal (pty) support".to_string());
        profile.push("(allow pseudo-tty)".to_string());
        profile.push("(allow file-ioctl".to_string());
        profile.push("  (literal \"/dev/ptmx\")".to_string());
        profile.push("  (regex #\"^/dev/ttys\")".to_string());
        profile.push(")".to_string());
        profile.push("(allow file-read* file-write*".to_string());
        profile.push("  (literal \"/dev/ptmx\")".to_string());
        profile.push("  (regex #\"^/dev/ttys\")".to_string());
        profile.push(")".to_string());
    }

    profile.join("\n")
}

/// The exact `(sysctl-name "...")` allowlist from
/// `macos-sandbox-utils.js:345-393`, reproduced verbatim and in order.
const SYSCTL_READ_NAMES: [&str; 49] = [
    "hw.activecpu",
    "hw.busfrequency_compat",
    "hw.byteorder",
    "hw.cacheconfig",
    "hw.cachelinesize_compat",
    "hw.cpufamily",
    "hw.cpufrequency",
    "hw.cpufrequency_compat",
    "hw.cputype",
    "hw.l1dcachesize_compat",
    "hw.l1icachesize_compat",
    "hw.l2cachesize_compat",
    "hw.l3cachesize_compat",
    "hw.logicalcpu",
    "hw.logicalcpu_max",
    "hw.machine",
    "hw.memsize",
    "hw.ncpu",
    "hw.nperflevels",
    "hw.packages",
    "hw.pagesize_compat",
    "hw.pagesize",
    "hw.physicalcpu",
    "hw.physicalcpu_max",
    "hw.tbfrequency_compat",
    "hw.vectorunit",
    "kern.argmax",
    "kern.bootargs",
    "kern.hostname",
    "kern.maxfiles",
    "kern.maxfilesperproc",
    "kern.maxproc",
    "kern.ngroups",
    "kern.osproductversion",
    "kern.osrelease",
    "kern.ostype",
    "kern.osvariant_status",
    "kern.osversion",
    "kern.secure_kernel",
    "kern.tcsm_available",
    "kern.tcsm_enable",
    "kern.usrstack64",
    "kern.version",
    "kern.willshutdown",
    "machdep.cpu.brand_string",
    "machdep.ptrauth_enabled",
    "security.mac.lockdown_mode_state",
    "sysctl.proc_cputype",
    "vm.loadavg",
];

/// The exact `(sysctl-name-prefix "...")` allowlist from
/// `macos-sandbox-utils.js:394-402`, reproduced verbatim and in order.
const SYSCTL_READ_PREFIXES: [&str; 9] = [
    "hw.optional.arm",
    "hw.optional.arm.",
    "hw.optional.armv8_",
    "hw.perflevel",
    "kern.proc.all",
    "kern.proc.pgrp.",
    "kern.proc.pid.",
    "machdep.cpu.",
    "net.routetable.",
];

/// Shell-quote + space-join a list the way the TS `shellquote.quote([...])`
/// does. Mirrors `shlex::try_join`, with a NUL-only fallback (a NUL cannot occur
/// for the paths/commands/profile here).
fn shjoin(parts: &[String]) -> String {
    shlex::try_join(parts.iter().map(String::as_str)).unwrap_or_else(|_| parts.join(" "))
}

/// Parameters for [`wrap_command_with_sandbox_macos`] — a 1:1 mirror of the TS
/// `wrapCommandWithSandboxMacOS` `params` object
/// (`macos-sandbox-utils.js:524-525`).
//
// The boolean flags are a faithful 1:1 transcription of the TS params object;
// collapsing them into enums would diverge from the reference shape.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default)]
pub struct WrapParams<'a> {
    /// The user command to wrap.
    pub command: &'a str,
    /// Whether to restrict the network.
    pub needs_network_restriction: bool,
    /// HTTP proxy port.
    pub http_proxy_port: Option<u16>,
    /// SOCKS proxy port.
    pub socks_proxy_port: Option<u16>,
    /// CA cert path threaded into the proxy env vars (TLS-MITM trust).
    pub ca_cert_path: Option<&'a str>,
    /// Specific Unix-socket subpaths to allow.
    pub allow_unix_sockets: Option<&'a [String]>,
    /// Allow all Unix sockets.
    pub allow_all_unix_sockets: bool,
    /// Allow local-IP binding.
    pub allow_local_binding: bool,
    /// User-specified XPC/Mach service names.
    pub allow_mach_lookup: Option<&'a [String]>,
    /// Read-restriction config (`denyOnly`).
    pub read_config: Option<&'a ReadConfig>,
    /// Write-restriction config (`allowOnly`).
    pub write_config: Option<&'a WriteConfig>,
    /// Allow pseudo-terminal (pty) support.
    pub allow_pty: bool,
    /// Allow git config in the sandbox.
    pub allow_git_config: bool,
    /// Enable weaker network isolation (trustd.agent mach-lookup).
    pub enable_weaker_network_isolation: bool,
    /// Allow Apple Events.
    pub allow_apple_events: bool,
    /// Shell to run the command with (`binShell || 'bash'`).
    pub bin_shell: Option<&'a str>,
    /// Resolved tmpdir for `generate_proxy_env_vars` (`TMPDIR` env var). In the
    /// TS this is resolved inside `generateProxyEnvVars`; here the caller
    /// resolves it (mirroring [`generate_proxy_env_vars`]'s pure signature).
    pub tmpdir: &'a str,
    /// Optional per-session token that validates per-command proxy credentials.
    pub proxy_auth_token: Option<&'a str>,
}

/// Wrap a command with the macOS Seatbelt sandbox.
///
/// Builds the SBPL profile via [`generate_sandbox_profile`] and returns the
/// shell-quoted `env <PROXY_ENV...> /usr/bin/sandbox-exec -p <profile> <shell> -c
/// <command>` invocation. The profile is passed inline via `-p` (matching the TS,
/// which uses `-p profile`, NOT a temp `.sb` file). When no restrictions apply
/// (no network restriction, no read denies, no write config) the bare command is
/// returned unchanged.
///
/// `which_sync` resolves the shell on `PATH`; the proxy env vars come from
/// [`generate_proxy_env_vars`] with [`Platform::Macos`].
///
/// Ported from `macos-sandbox-utils.js:524-585` (`wrapCommandWithSandboxMacOS`).
///
/// # Errors
/// Returns an `io::Error` if the requested shell cannot be resolved on `PATH`
/// (faithful to the TS `throw new Error("Shell '...' not found in PATH")`).
pub fn wrap_command_with_sandbox_macos(params: &WrapParams<'_>) -> std::io::Result<String> {
    // Read: denyOnly pattern — empty array means no restrictions.
    let has_read_restrictions = params
        .read_config
        .is_some_and(|rc| !rc.deny_only.is_empty());
    // Write: allowOnly pattern — None means no restrictions, any config = restrictions.
    let has_write_restrictions = params.write_config.is_some();

    // No sandboxing needed.
    if !params.needs_network_restriction && !has_read_restrictions && !has_write_restrictions {
        return Ok(params.command.to_string());
    }

    let log_tag = generate_log_tag(params.command);
    let profile = generate_sandbox_profile(&ProfileParams {
        read_config: params.read_config,
        write_config: params.write_config,
        http_proxy_port: params.http_proxy_port,
        socks_proxy_port: params.socks_proxy_port,
        needs_network_restriction: params.needs_network_restriction,
        allow_unix_sockets: params.allow_unix_sockets,
        allow_all_unix_sockets: params.allow_all_unix_sockets,
        allow_local_binding: params.allow_local_binding,
        allow_mach_lookup: params.allow_mach_lookup,
        allow_pty: params.allow_pty,
        allow_git_config: params.allow_git_config,
        enable_weaker_network_isolation: params.enable_weaker_network_isolation,
        allow_apple_events: params.allow_apple_events,
        log_tag: &log_tag,
    });

    // Proxy env vars (KEY=value strings) via the shared utility.
    let proxy_env_args: Vec<String> = crate::env::generate_proxy_env_vars_with_auth(
        params.http_proxy_port,
        params.socks_proxy_port,
        params.ca_cert_path,
        Platform::Macos,
        params.tmpdir,
        Some(params.command),
        params.proxy_auth_token,
    )
    .into_iter()
    .map(|(k, v)| format!("{k}={v}"))
    .collect();

    // Resolve the shell on PATH (the TS `whichSync(shellName)`).
    let shell_name = params.bin_shell.unwrap_or("bash");
    let shell = which::which(shell_name)
        .map_err(|_| std::io::Error::other(format!("Shell '{shell_name}' not found in PATH")))?;
    let shell = shell.to_string_lossy().into_owned();

    // shellquote.quote(['env', ...proxyEnvArgs, '/usr/bin/sandbox-exec', '-p',
    // profile, shell, '-c', command]).
    let mut argv: Vec<String> = Vec::with_capacity(proxy_env_args.len() + 7);
    argv.push("env".to_string());
    argv.extend(proxy_env_args);
    argv.push("/usr/bin/sandbox-exec".to_string());
    argv.push("-p".to_string());
    argv.push(profile);
    argv.push(shell);
    argv.push("-c".to_string());
    argv.push(params.command.to_string());

    Ok(shjoin(&argv))
}

/// A sandbox violation surfaced by [`start_macos_sandbox_log_monitor`].
///
/// Mirrors the object the TS `callback` receives
/// (`macos-sandbox-utils.js:659-664`): the violation detail line, the decoded
/// command (if recoverable), its base64 form, and a timestamp.
#[derive(Debug, Clone)]
pub struct LogViolation {
    /// The violation detail text (the part after `Sandbox: `).
    pub line: String,
    /// The decoded command, if a `CMD64_...` line was present and decodable.
    pub command: Option<String>,
    /// The base64-encoded command (the `CMD64_<this>_END` capture).
    pub encoded_command: Option<String>,
    /// Time the violation was observed (set when the line is processed).
    pub timestamp: std::time::SystemTime,
}

/// Parse a chunk of `log stream` output into a [`LogViolation`], applying the
/// same filtering the TS `startMacOSSandboxLogMonitor` data handler does
/// (`macos-sandbox-utils.js:608-664`). Returns `None` when the chunk carries no
/// reportable violation (no `Sandbox: ... deny` line, an un-parseable detail, a
/// noisy mDNS/diagnosticd/analyticsd violation, or an ignore-list match).
///
/// `ignore_violations` mirrors the TS map: the `"*"` key holds wildcard path
/// substrings checked for every command; other keys are command substrings whose
/// value is a list of path substrings to ignore when the command matches.
///
/// Pulled out as a pure function so the filtering logic is unit-testable without
/// spawning `log stream`.
// The ignore-map mirrors the TS plain object; a fixed `HashMap` keeps the public
// shape simple (generalizing the hasher would leak a type param for no benefit).
#[allow(clippy::implicit_hasher)]
#[must_use]
pub fn parse_violation_chunk(
    chunk: &str,
    ignore_violations: Option<&std::collections::HashMap<String, Vec<String>>>,
) -> Option<LogViolation> {
    let lines: Vec<&str> = chunk.split('\n').collect();

    // Violation line: contains "Sandbox:" AND "deny".
    let violation_line = lines
        .iter()
        .find(|l| l.contains("Sandbox:") && l.contains("deny"))?;
    // Command line: starts with "CMD64_".
    let command_line = lines.iter().find(|l| l.starts_with("CMD64_"));

    // Extract violation details: /Sandbox:\s+(.+)$/.
    let violation_details = {
        let idx = violation_line.find("Sandbox:")?;
        let rest = &violation_line[idx + "Sandbox:".len()..];
        let trimmed = rest.trim_start_matches([' ', '\t']);
        if trimmed.is_empty() {
            return None;
        }
        // (.+)$ — to end of line (the line is already a single log line).
        trimmed.to_string()
    };

    // Try to recover the command: /CMD64_(.+?)_END/.
    let mut command: Option<String> = None;
    let mut encoded_command: Option<String> = None;
    if let Some(cmd_line) = command_line {
        if let Some(start) = cmd_line.find("CMD64_") {
            let after = &cmd_line[start + "CMD64_".len()..];
            if let Some(end) = after.find("_END") {
                let enc = &after[..end];
                encoded_command = Some(enc.to_string());
                let decoded = crate::env::decode_sandboxed_command(enc);
                if !decoded.is_empty() {
                    command = Some(decoded);
                }
            }
        }
    }

    // Always filter out noisy violations.
    if violation_details.contains("mDNSResponder")
        || violation_details.contains("mach-lookup com.apple.diagnosticd")
        || violation_details.contains("mach-lookup com.apple.analyticsd")
    {
        return None;
    }

    // Ignore-list filtering (only when we recovered a command).
    if let (Some(ignore), Some(cmd)) = (ignore_violations, command.as_ref()) {
        // Wildcard patterns first.
        if let Some(wildcard_paths) = ignore.get("*") {
            if wildcard_paths.iter().any(|p| violation_details.contains(p)) {
                return None;
            }
        }
        // Command-specific patterns.
        for (pattern, paths) in ignore {
            if pattern == "*" {
                continue;
            }
            if cmd.contains(pattern) && paths.iter().any(|p| violation_details.contains(p)) {
                return None;
            }
        }
    }

    Some(LogViolation {
        line: violation_details,
        command,
        encoded_command,
        timestamp: std::time::SystemTime::now(),
    })
}

/// Start monitoring macOS system logs for sandbox violations.
///
/// Spawns `log stream --predicate '(eventMessage ENDSWITH "<sessionSuffix>")'
/// --style compact` and feeds each chunk through [`parse_violation_chunk`]; every
/// surfaced [`LogViolation`] is handed to `callback`. Returns a join handle plus
/// a stop closure that kills the `log` process (mirroring the TS returned
/// `() => logProcess.kill('SIGTERM')`).
///
/// macOS-gated because it shells `log stream` (unavailable elsewhere). The pure
/// parsing/filtering lives in [`parse_violation_chunk`] and is tested on every
/// platform.
///
/// Ported from `macos-sandbox-utils.js:590-678` (`startMacOSSandboxLogMonitor`).
#[cfg(target_os = "macos")]
#[allow(clippy::implicit_hasher)]
pub fn start_macos_sandbox_log_monitor<F>(
    callback: F,
    ignore_violations: Option<std::collections::HashMap<String, Vec<String>>>,
) -> std::io::Result<MacosLogMonitor>
where
    F: Fn(LogViolation) + Send + 'static,
{
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    let predicate = format!("(eventMessage ENDSWITH \"{}\")", session_suffix());
    let mut child = Command::new("log")
        .args(["stream", "--predicate", &predicate, "--style", "compact"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("log stream: no stdout"))?;

    // The TS handler splits each `data` chunk on newlines and looks for a
    // Sandbox line + an adjacent CMD64 line. `log --style compact` emits one
    // record per line; we accumulate a small rolling window so a CMD64 line and
    // its Sandbox line in the same flush are seen together.
    let handle = std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut window: Vec<String> = Vec::new();
        for line in reader.lines().map_while(Result::ok) {
            window.push(line);
            if window.len() > 8 {
                window.remove(0);
            }
            let chunk = window.join("\n");
            if let Some(v) = parse_violation_chunk(&chunk, ignore_violations.as_ref()) {
                callback(v);
                window.clear();
            }
        }
    });

    Ok(MacosLogMonitor {
        child,
        handle: Some(handle),
    })
}

/// Handle for a running [`start_macos_sandbox_log_monitor`]. Call [`MacosLogMonitor::stop`]
/// (or drop) to terminate the underlying `log stream` process.
#[cfg(target_os = "macos")]
#[derive(Debug)]
pub struct MacosLogMonitor {
    child: std::process::Child,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "macos")]
impl MacosLogMonitor {
    /// Kill the `log stream` process and join the reader thread (mirrors the TS
    /// returned `() => logProcess.kill('SIGTERM')`).
    pub fn stop(mut self) {
        let _ = self.child.kill();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for MacosLogMonitor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod profile_text_tests {
    use super::*;

    fn rc(deny: &[&str], allow: &[&str]) -> ReadConfig {
        ReadConfig {
            deny_only: deny.iter().map(ToString::to_string).collect(),
            allow_within_deny: allow.iter().map(ToString::to_string).collect(),
        }
    }

    fn wc(allow: &[&str], deny: &[&str]) -> WriteConfig {
        WriteConfig {
            allow_only: allow.iter().map(ToString::to_string).collect(),
            deny_within_allow: deny.iter().map(ToString::to_string).collect(),
        }
    }

    // --- escape_path (macos-sandbox-utils.js:515-520) ---

    #[test]
    fn escape_path_matches_json_stringify() {
        assert_eq!(escape_path("/a/b"), "\"/a/b\"");
        // Spaces are not escaped, just kept inside quotes.
        assert_eq!(escape_path("/a b/c"), "\"/a b/c\"");
        // Double quotes are backslash-escaped.
        assert_eq!(escape_path("/a\"b"), "\"/a\\\"b\"");
        // Backslashes are doubled.
        assert_eq!(escape_path("/a\\b"), "\"/a\\\\b\"");
        // A regex with ^ and $ — JSON.stringify leaves them untouched.
        assert_eq!(escape_path("^/a/[^/]*$"), "\"^/a/[^/]*$\"");
    }

    // --- mac_get_mandatory_deny_patterns (macos-sandbox-utils.js:11-33) ---

    #[test]
    fn mandatory_deny_patterns_shape_and_dedup() {
        let out = mac_get_mandatory_deny_patterns_with(false, "/proj");
        // Dangerous file: absolute CWD path + glob.
        assert!(out.contains(&"/proj/.gitconfig".to_string()));
        assert!(out.contains(&"**/.gitconfig".to_string()));
        // Dangerous dir: absolute + glob/**.
        assert!(out.contains(&"/proj/.vscode".to_string()));
        assert!(out.contains(&"**/.vscode/**".to_string()));
        assert!(out.contains(&"/proj/.lingxi/commands".to_string()));
        assert!(out.contains(&"**/.lingxi/commands/**".to_string()));
        // Git hooks always blocked.
        assert!(out.contains(&"/proj/.git/hooks".to_string()));
        assert!(out.contains(&"**/.git/hooks/**".to_string()));
        // Git config blocked when !allow_git_config.
        assert!(out.contains(&"/proj/.git/config".to_string()));
        assert!(out.contains(&"**/.git/config".to_string()));
        // Deduped.
        let mut sorted = out.clone();
        sorted.sort();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(before, sorted.len());
    }

    #[test]
    fn mandatory_deny_patterns_allow_git_config_drops_config() {
        let out = mac_get_mandatory_deny_patterns_with(true, "/proj");
        assert!(out.contains(&"/proj/.git/hooks".to_string()));
        assert!(!out.contains(&"/proj/.git/config".to_string()));
        assert!(!out.contains(&"**/.git/config".to_string()));
    }

    // --- get_ancestor_directories (macos-sandbox-utils.js:47-61) ---

    #[test]
    fn ancestor_directories_walk_to_root() {
        assert_eq!(
            get_ancestor_directories("/private/tmp/test/file.txt"),
            vec![
                "/private/tmp/test".to_string(),
                "/private/tmp".to_string(),
                "/private".to_string(),
            ]
        );
    }

    // --- generate_read_rules ordering (macos-sandbox-utils.js:134-214) ---

    #[test]
    fn read_rules_none_allows_all() {
        assert_eq!(
            generate_read_rules(None, "TAG", None),
            vec!["(allow file-read*)".to_string()]
        );
    }

    #[test]
    fn read_rules_deny_then_reallow_in_order() {
        // Literal (non-glob) paths -> subpath matching, deterministic order.
        let config = rc(&["/x"], &["/x/y"]);
        let rules = generate_read_rules(Some(&config), "TAG", None);
        let joined = rules.join("\n");
        // Default allow first.
        assert_eq!(rules[0], "(allow file-read*)");
        // Deny /x then re-allow /x/y, in that order.
        let deny_pos = joined.find("(deny file-read*\n  (subpath \"/x\")").unwrap();
        let allow_pos = joined
            .find("(allow file-read*\n  (subpath \"/x/y\")")
            .unwrap();
        assert!(deny_pos < allow_pos, "deny must precede the re-allow");
        // Directory metadata rule emitted because deny_only is non-empty.
        assert!(joined.contains("(allow file-read-metadata\n  (vnode-type DIRECTORY))"));
    }

    #[test]
    fn read_rules_redeny_nested_path_inside_reallow() {
        // v0d's second deny pass: a nested deny under a re-allowed subpath must be
        // RE-DENIED after the broader re-allow, or Seatbelt's last-rule-wins would
        // leak read access to the nested deny. Order: deny /x, deny /x/secret,
        // allow /x, deny /x/secret (again).
        let config = rc(&["/x", "/x/secret"], &["/x"]);
        let rules = generate_read_rules(Some(&config), "TAG", None);
        let joined = rules.join("\n");
        let reallow = joined
            .find("(allow file-read*\n  (subpath \"/x\")")
            .unwrap();
        let redeny = joined
            .rfind("(deny file-read*\n  (subpath \"/x/secret\")")
            .unwrap();
        assert!(
            redeny > reallow,
            "the nested deny must be re-applied AFTER the re-allow:\n{joined}"
        );
        // A non-nested deny (/x itself) is NOT re-denied (it is not inside /x/).
        let x_denies = joined
            .matches("(deny file-read*\n  (subpath \"/x\")\n")
            .count();
        assert_eq!(
            x_denies, 1,
            "/x should be denied once, not re-denied:\n{joined}"
        );
    }

    #[test]
    fn read_rules_glob_uses_regex() {
        let config = rc(&["/x/*.env"], &[]);
        let rules = generate_read_rules(Some(&config), "TAG", None);
        let joined = rules.join("\n");
        // A glob path -> (deny file-read* (regex ...)).
        assert!(joined.contains("(deny file-read*\n  (regex "));
        // The regex is the glob_to_regex output, JSON-escaped.
        let expected = escape_path(&glob_to_regex(&normalize_path_for_sandbox("/x/*.env")));
        assert!(joined.contains(&format!("(regex {expected})")));
    }

    #[test]
    fn read_rules_deny_root_reallows_literal_root() {
        let config = rc(&["/"], &[]);
        let rules = generate_read_rules(Some(&config), "TAG", None);
        let joined = rules.join("\n");
        assert!(joined.contains("(allow file-read* (literal \"/\"))"));
    }

    #[test]
    fn read_rules_write_allow_reallows_unlink_create() {
        let config = rc(&["/x"], &[]);
        let write_allow = vec!["/w".to_string()];
        let rules = generate_read_rules(Some(&config), "TAG", Some(&write_allow));
        let joined = rules.join("\n");
        assert!(joined.contains("(allow file-write-unlink\n  (subpath \"/w\")"));
        assert!(joined.contains("(allow file-write-create\n  (subpath \"/w\")"));
    }

    // --- generate_write_rules (macos-sandbox-utils.js:218-256) ---

    #[test]
    fn write_rules_none_allows_all() {
        assert_eq!(
            generate_write_rules(None, "TAG", false),
            vec!["(allow file-write*)".to_string()]
        );
    }

    #[test]
    fn write_rules_allow_then_deny_then_mandatory() {
        let config = wc(&["/w"], &["/w/d"]);
        let rules = generate_write_rules(Some(&config), "TAG", false);
        let joined = rules.join("\n");
        // Allow /w.
        let allow_pos = joined
            .find("(allow file-write*\n  (subpath \"/w\")")
            .unwrap();
        // Deny /w/d.
        let deny_pos = joined
            .find("(deny file-write*\n  (subpath \"/w/d\")")
            .unwrap();
        assert!(allow_pos < deny_pos, "allow must precede the deny");
        // Mandatory deny patterns present (e.g. the **/.git/hooks/** glob -> regex).
        // The real pipeline normalizes the pattern before glob_to_regex.
        let mand_regex = escape_path(&glob_to_regex(&normalize_path_for_sandbox(
            "**/.git/hooks/**",
        )));
        assert!(
            joined.contains(&format!("(deny file-write*\n  (regex {mand_regex})")),
            "mandatory **/.git/hooks/** deny must appear"
        );
    }

    // --- generate_sandbox_profile static header + sections ---

    #[test]
    fn profile_has_verbatim_header_and_sysctl_allowlist() {
        let params = ProfileParams {
            log_tag: "MYTAG",
            ..Default::default()
        };
        let profile = generate_sandbox_profile(&params);
        assert!(profile.starts_with("(version 1)\n(deny default (with message \"MYTAG\"))"));
        assert!(profile.contains("; LogTag: MYTAG"));
        assert!(profile.contains("(allow process-exec)"));
        assert!(profile.contains("(global-name \"com.apple.coreservices.launchservicesd\")"));
        // Full sysctl allowlist - spot-check first/last names + a prefix.
        assert!(profile.contains("  (sysctl-name \"hw.activecpu\")"));
        assert!(profile.contains("  (sysctl-name \"vm.loadavg\")"));
        assert!(profile.contains("  (sysctl-name-prefix \"net.routetable.\")"));
        // system-socket AF_SYSTEM rule verbatim.
        assert!(profile.contains(
            "(allow system-socket (require-all (socket-domain AF_SYSTEM) (socket-protocol 2)))"
        ));
    }

    #[test]
    fn profile_network_unrestricted_allows_all() {
        let params = ProfileParams {
            needs_network_restriction: false,
            log_tag: "T",
            ..Default::default()
        };
        let profile = generate_sandbox_profile(&params);
        assert!(profile.contains("; Network\n(allow network*)"));
    }

    #[test]
    fn profile_network_restricted_with_proxy_and_sockets() {
        let unix = vec!["/var/run/docker.sock".to_string()];
        let params = ProfileParams {
            needs_network_restriction: true,
            http_proxy_port: Some(3128),
            socks_proxy_port: Some(1080),
            allow_local_binding: true,
            allow_unix_sockets: Some(&unix),
            log_tag: "T",
            ..Default::default()
        };
        let profile = generate_sandbox_profile(&params);
        assert!(!profile.contains("(allow network*)"));
        assert!(profile.contains("(allow network-bind (local ip \"*:*\"))"));
        assert!(profile.contains("(allow network-bind (local ip \"localhost:3128\"))"));
        assert!(profile.contains("(allow network-outbound (remote ip \"localhost:3128\"))"));
        assert!(profile.contains("(allow network-bind (local ip \"localhost:1080\"))"));
        assert!(profile.contains("(allow system-socket (socket-domain AF_UNIX))"));
        // Unix socket subpath (normalize may resolve symlinks; check the verb shape).
        assert!(profile.contains("(allow network-bind (local unix-socket (subpath "));
    }

    #[test]
    fn profile_all_unix_sockets_uses_path_regex() {
        let params = ProfileParams {
            needs_network_restriction: true,
            allow_all_unix_sockets: true,
            log_tag: "T",
            ..Default::default()
        };
        let profile = generate_sandbox_profile(&params);
        assert!(profile.contains("(allow network-bind (local unix-socket (path-regex #\"^/\")))"));
        assert!(
            profile.contains("(allow network-outbound (remote unix-socket (path-regex #\"^/\")))")
        );
    }

    #[test]
    fn profile_conditional_sections() {
        let mach = vec![
            "com.example.foo".to_string(),
            "com.example.bar.*".to_string(),
        ];
        let params = ProfileParams {
            enable_weaker_network_isolation: true,
            allow_apple_events: true,
            allow_mach_lookup: Some(&mach),
            allow_pty: true,
            log_tag: "T",
            ..Default::default()
        };
        let profile = generate_sandbox_profile(&params);
        assert!(profile.contains("(allow mach-lookup (global-name \"com.apple.trustd.agent\"))"));
        assert!(profile.contains("(allow appleevent-send)"));
        assert!(profile.contains("(allow lsopen)"));
        // Trailing-* user mach service -> global-name-prefix; plain -> global-name.
        assert!(profile.contains("(allow mach-lookup (global-name \"com.example.foo\"))"));
        assert!(profile.contains("(allow mach-lookup (global-name-prefix \"com.example.bar.\"))"));
        // pty block.
        assert!(profile.contains("; Pseudo-terminal (pty) support"));
        assert!(profile.contains("(allow pseudo-tty)"));
        assert!(profile.contains("  (regex #\"^/dev/ttys\")"));
    }

    #[test]
    fn profile_read_before_write_section() {
        let r = rc(&["/x"], &[]);
        let w = wc(&["/w"], &[]);
        let params = ProfileParams {
            read_config: Some(&r),
            write_config: Some(&w),
            log_tag: "T",
            ..Default::default()
        };
        let profile = generate_sandbox_profile(&params);
        let read_pos = profile.find("; File read").unwrap();
        let write_pos = profile.find("; File write").unwrap();
        assert!(read_pos < write_pos, "read rules must precede write rules");
    }

    #[test]
    fn sysctl_const_lengths_consistent() {
        // 49 names + 9 prefixes, matching the verbatim TS allowlist.
        assert_eq!(SYSCTL_READ_NAMES.len(), 49);
        assert_eq!(SYSCTL_READ_PREFIXES.len(), 9);
    }

    // --- wrap_command_with_sandbox_macos (macos-sandbox-utils.js:524-585) ---

    #[test]
    fn wrap_no_restrictions_returns_bare_command() {
        let params = WrapParams {
            command: "echo hi",
            needs_network_restriction: false,
            tmpdir: "/tmp/claude",
            ..Default::default()
        };
        assert_eq!(wrap_command_with_sandbox_macos(&params).unwrap(), "echo hi");
    }

    #[test]
    fn wrap_write_restriction_builds_sandbox_exec_invocation() {
        let wcfg = wc(&["/tmp/work"], &[]);
        let params = WrapParams {
            command: "echo hi",
            write_config: Some(&wcfg),
            // sh is on PATH everywhere; avoids depending on bash location.
            bin_shell: Some("sh"),
            tmpdir: "/tmp/claude",
            ..Default::default()
        };
        let out = wrap_command_with_sandbox_macos(&params).unwrap();
        // env <PROXY_ENV...> /usr/bin/sandbox-exec -p <profile> <shell> -c <command>.
        assert!(out.starts_with("env "), "must start with env; got {out}");
        assert!(out.contains("/usr/bin/sandbox-exec"));
        assert!(out.contains("-p"));
        // The TMPDIR proxy env var is set (no proxy ports -> minimal env).
        assert!(out.contains("TMPDIR=/tmp/claude"));
        assert!(out.contains("SANDBOX_RUNTIME=1"));
        // The command tail is shell-quoted and present.
        assert!(out.contains("echo hi") || out.contains("'echo hi'"));
    }

    #[test]
    fn wrap_network_restriction_threads_proxy_env() {
        let params = WrapParams {
            command: "curl example.com",
            needs_network_restriction: true,
            http_proxy_port: Some(3128),
            bin_shell: Some("sh"),
            tmpdir: "/tmp/claude",
            ..Default::default()
        };
        let out = wrap_command_with_sandbox_macos(&params).unwrap();
        // Proxy env vars woven in.
        assert!(out.contains("HTTP_PROXY=http://localhost:3128"));
        assert!(out.contains("/usr/bin/sandbox-exec"));
    }

    #[test]
    fn wrap_unknown_shell_errors() {
        let wcfg = wc(&["/tmp/work"], &[]);
        let params = WrapParams {
            command: "echo hi",
            write_config: Some(&wcfg),
            bin_shell: Some("definitely-not-a-real-shell-binary-xyz"),
            tmpdir: "/tmp/claude",
            ..Default::default()
        };
        assert!(wrap_command_with_sandbox_macos(&params).is_err());
    }

    // --- parse_violation_chunk (macos-sandbox-utils.js:608-664) ---

    #[test]
    fn parse_violation_extracts_details_and_command() {
        let enc = encode_sandboxed_command("rm -rf /etc/passwd");
        let chunk = format!(
            "CMD64_{enc}_END_xyz_SBX\nkernel Sandbox: bash(123) deny file-write /etc/passwd"
        );
        let v = parse_violation_chunk(&chunk, None).expect("violation");
        assert!(v.line.contains("deny file-write /etc/passwd"));
        assert_eq!(v.encoded_command.as_deref(), Some(enc.as_str()));
        assert_eq!(v.command.as_deref(), Some("rm -rf /etc/passwd"));
    }

    #[test]
    fn parse_violation_none_without_sandbox_deny_line() {
        let v = parse_violation_chunk("some unrelated log line", None);
        assert!(v.is_none());
    }

    #[test]
    fn parse_violation_filters_noisy() {
        let chunk = "Sandbox: foo(1) deny mach-lookup com.apple.diagnosticd";
        assert!(parse_violation_chunk(chunk, None).is_none());
        let chunk2 = "Sandbox: foo(1) deny network-outbound mDNSResponder";
        assert!(parse_violation_chunk(chunk2, None).is_none());
    }

    #[test]
    fn parse_violation_wildcard_ignore_match() {
        let enc = encode_sandboxed_command("git status");
        let chunk = format!("CMD64_{enc}_END_z_SBX\nSandbox: git(1) deny file-read /private/var/x");
        let mut ignore = std::collections::HashMap::new();
        ignore.insert("*".to_string(), vec!["/private/var/x".to_string()]);
        assert!(parse_violation_chunk(&chunk, Some(&ignore)).is_none());
        // A non-matching wildcard path does not suppress it.
        let mut ignore2 = std::collections::HashMap::new();
        ignore2.insert("*".to_string(), vec!["/some/other".to_string()]);
        assert!(parse_violation_chunk(&chunk, Some(&ignore2)).is_some());
    }

    #[test]
    fn parse_violation_command_specific_ignore_match() {
        let enc = encode_sandboxed_command("npm install");
        let chunk =
            format!("CMD64_{enc}_END_z_SBX\nSandbox: node(1) deny file-read /home/u/.npmrc");
        let mut ignore = std::collections::HashMap::new();
        ignore.insert("npm".to_string(), vec![".npmrc".to_string()]);
        assert!(parse_violation_chunk(&chunk, Some(&ignore)).is_none());
    }
}

// ===== macOS runtime gate: these RUN on this host via sandbox-exec. =====
#[cfg(all(test, target_os = "macos"))]
mod runtime_gate_tests {
    use super::*;
    use std::process::Command;

    /// Run `sandbox-exec -p <profile> <argv...>`, returning whether it exited 0.
    fn sandbox_exec_ok(profile: &str, argv: &[&str]) -> std::process::Output {
        let mut cmd = Command::new("/usr/bin/sandbox-exec");
        cmd.arg("-p").arg(profile);
        for a in argv {
            cmd.arg(a);
        }
        cmd.output().expect("spawn sandbox-exec")
    }

    fn wc(allow: &[&str], deny: &[&str]) -> WriteConfig {
        WriteConfig {
            allow_only: allow.iter().map(ToString::to_string).collect(),
            deny_within_allow: deny.iter().map(ToString::to_string).collect(),
        }
    }

    fn rc(deny: &[&str], allow: &[&str]) -> ReadConfig {
        ReadConfig {
            deny_only: deny.iter().map(ToString::to_string).collect(),
            allow_within_deny: allow.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn gate_a_generated_profile_compiles_and_runs_true() {
        // A non-trivial profile (network restriction + write config) must be
        // accepted by sandbox-exec: `sandbox-exec -p <profile> /usr/bin/true`
        // exits 0.
        let tmp = std::env::temp_dir().join("p9a-gate-a");
        let _ = std::fs::create_dir_all(&tmp);
        let wcfg = wc(&[&tmp.to_string_lossy()], &[]);
        let profile = generate_sandbox_profile(&ProfileParams {
            write_config: Some(&wcfg),
            needs_network_restriction: true,
            http_proxy_port: Some(3128),
            allow_local_binding: true,
            log_tag: "GATE_A",
            ..Default::default()
        });
        let out = sandbox_exec_ok(&profile, &["/usr/bin/true"]);
        assert!(
            out.status.success(),
            "sandbox-exec rejected the generated profile: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn gate_b_write_deny_blocks_write_to_denied_dir() {
        // A write-restricted profile that does NOT allow writes to <denied>
        // must BLOCK `echo x > <denied>/f`.
        let denied = std::env::temp_dir().join(format!("p9a-gate-b-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&denied);
        // allow_only points elsewhere (a different temp dir), so <denied> is
        // read-only root -> writes there fail.
        let allowed = std::env::temp_dir().join(format!("p9a-gate-b-allow-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&allowed);
        let wcfg = wc(&[&allowed.to_string_lossy()], &[]);
        let profile = generate_sandbox_profile(&ProfileParams {
            write_config: Some(&wcfg),
            log_tag: "GATE_B",
            ..Default::default()
        });
        let target = denied.join("f");
        let script = format!("echo x > {}", target.to_string_lossy());
        let out = sandbox_exec_ok(&profile, &["/bin/sh", "-c", &script]);
        assert!(
            !out.status.success(),
            "write to denied dir SUCCEEDED but should be blocked"
        );
        assert!(
            !target.exists(),
            "denied file must not have been created: {target:?}"
        );
        let _ = std::fs::remove_dir_all(&denied);
        let _ = std::fs::remove_dir_all(&allowed);
    }

    #[test]
    fn gate_c_read_allow_within_deny_works() {
        // Deny reads under <root>, but re-allow <root>/readable. Reading the
        // allowed file must SUCCEED; reading a sibling under the deny must FAIL.
        let root = std::env::temp_dir().join(format!("p9a-gate-c-{}", std::process::id()));
        let readable = root.join("readable");
        let hidden = root.join("hidden");
        let _ = std::fs::create_dir_all(&readable);
        let _ = std::fs::create_dir_all(&hidden);
        let ok_file = readable.join("ok.txt");
        let secret_file = hidden.join("secret.txt");
        std::fs::write(&ok_file, "OK").unwrap();
        std::fs::write(&secret_file, "SECRET").unwrap();

        // Canonicalize because normalize_path_for_sandbox resolves /var -> /private/var.
        let root_c = std::fs::canonicalize(&root).unwrap();
        let readable_c = std::fs::canonicalize(&readable).unwrap();
        let rcfg = rc(
            &[&root_c.to_string_lossy()],
            &[&readable_c.to_string_lossy()],
        );
        let profile = generate_sandbox_profile(&ProfileParams {
            read_config: Some(&rcfg),
            log_tag: "GATE_C",
            ..Default::default()
        });

        let ok_c = std::fs::canonicalize(&ok_file).unwrap();
        let secret_c = std::fs::canonicalize(&secret_file).unwrap();
        let read_ok = sandbox_exec_ok(&profile, &["/bin/cat", &ok_c.to_string_lossy()]);
        assert!(
            read_ok.status.success(),
            "read of allow-within-deny file failed: stderr={}",
            String::from_utf8_lossy(&read_ok.stderr)
        );
        let read_secret = sandbox_exec_ok(&profile, &["/bin/cat", &secret_c.to_string_lossy()]);
        assert!(
            !read_secret.status.success(),
            "read of a denied-region file SUCCEEDED but should be blocked"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn gate_d_wrap_command_invocation_runs_end_to_end() {
        // The full wrap_command_with_sandbox_macos invocation must run via the
        // user's shell: build it, then execute it with `sh -c <invocation>`.
        let allowed = std::env::temp_dir().join(format!("p9a-gate-d-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&allowed);
        let wcfg = wc(&[&allowed.to_string_lossy()], &[]);
        let params = WrapParams {
            command: "/usr/bin/true",
            write_config: Some(&wcfg),
            bin_shell: Some("sh"),
            tmpdir: "/tmp/claude",
            ..Default::default()
        };
        let invocation = wrap_command_with_sandbox_macos(&params).unwrap();
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(&invocation)
            .output()
            .expect("run wrapped invocation");
        assert!(
            out.status.success(),
            "wrapped sandbox-exec invocation failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = std::fs::remove_dir_all(&allowed);
    }
}
