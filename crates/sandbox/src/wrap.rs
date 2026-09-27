//! Sandbox wrap dispatch: produce a shell-runnable wrapped command line per
//! platform.
//!
//! - Linux / WSL2: `bwrap` with policy-derived filesystem + network flags,
//!   suffixed by `-- /bin/sh -c '<command>'`. A `socat` companion to enforce
//!   `network.allowed_domains` filtering is wired up as a process-lifecycle
//!   concern at the `platforms/posix/src/sandbox.rs` layer (Task 17); this
//!   builder just emits the `bwrap` argv that establishes a netns. See
//!   `TODO(M2-followup)` notes inline.
//! - macOS: write an SBPL profile to a tempfile, return
//!   `sandbox-exec -f <profile> <command>`.
//! - Windows / WSL1: return [`SandboxWrapError::Unsupported`] (handled by the
//!   refusal path before this dispatcher runs).

use crate::runtime_config::{Platform, SandboxRuntimeConfig};
use sandbox_runtime::fs_args::{ReadConfig, WriteConfig};
use sandbox_runtime::get_default_write_paths;
use sandbox_runtime::macos::{generate_sandbox_profile, ProfileParams};
use sandbox_runtime::path_utils::remove_trailing_glob_suffix;
use thiserror::Error;

/// Errors produced by [`wrap_with_sandbox`].
#[derive(Debug, Clone, Error)]
pub enum SandboxWrapError {
    /// Platform is on the refuse list (Windows, WSL1) — the caller should
    /// surface the inner string verbatim.
    #[error("{0}")]
    Unsupported(String),
    /// Failed to write the SBPL profile tempfile.
    #[error("failed to write SBPL profile: {0}")]
    SbplWrite(String),
}

/// Wrap `command` for execution under the sandbox identified by `platform`.
///
/// `policy` carries the runtime configuration (filesystem / network /
/// excludedCommands etc.).
///
/// Returns the shell-runnable string the caller should pass to its
/// `/bin/sh -c` runner.
///
/// # Errors
/// Returns [`SandboxWrapError::SbplWrite`] when the macOS path cannot persist
/// the SBPL profile tempfile. The Linux / WSL2 path is infallible. Windows /
/// WSL1 should be handled by the refusal pipeline upstream — see
/// [`crate::dependency_check::sandbox_unavailable_reason`].
pub fn wrap_with_sandbox(
    command: &str,
    policy: &SandboxRuntimeConfig,
    platform: Platform,
) -> Result<String, SandboxWrapError> {
    match platform {
        Platform::Linux | Platform::Wsl => Ok(wrap_linux_bwrap(command, policy)),
        Platform::Mac => wrap_macos_sbpl(command, policy),
    }
}

// =============================================================================
// Linux / WSL2 — bwrap
// =============================================================================

/// Build the `bwrap` invocation. The caller (`platforms/posix/src/sandbox.rs`)
/// passes this string to `/bin/sh -c` and is responsible for spawning the
/// optional `socat` companion when `network.allowed_domains` is non-empty.
fn wrap_linux_bwrap(command: &str, policy: &SandboxRuntimeConfig) -> String {
    let filesystem_disabled = policy.filesystem.disabled;
    // Default ro root + tmpfs ephemeral writes + procfs/devfs + pid-ns +
    // die-with-parent — match claude-code's defaults.
    let mut args: Vec<String> = vec![
        // Create a user namespace where possible; degrade gracefully on kernels
        // with unprivileged userns disabled (`-try`) instead of failing to start
        // (finding 5). Without it an unprivileged bwrap cannot create the pid/net
        // namespaces below. Verified to start + degrade under user.max_user_namespaces=0.
        "--unshare-user-try".into(),
        if filesystem_disabled {
            "--bind".into()
        } else {
            "--ro-bind".into()
        },
        "/".into(),
        "/".into(),
        "--tmpfs".into(),
        "/tmp".into(),
        "--proc".into(),
        "/proc".into(),
        "--dev".into(),
        "/dev".into(),
        "--unshare-pid".into(),
        "--die-with-parent".into(),
    ];

    // Writable paths per filesystem.allow_write.
    if !filesystem_disabled {
        for path in &policy.filesystem.allow_write {
            args.push("--bind".into());
            args.push(path.clone());
            args.push(path.clone());
        }
    }

    // Deny-write: re-mount existing denied / bare-repo paths read-only IN PLACE.
    // Placed after the allow_write `--bind`s so a deny overrides a writable
    // parent (bwrap: later mounts win — verified). NEVER `--ro-bind-try /dev/null`
    // (that blanks the host file); ro-bind-in-place preserves it read-only
    // (finding 3, sandbox-adapter.ts:264).
    if !filesystem_disabled {
        for path in &policy.ro_bind_in_place {
            args.push("--ro-bind".into());
            args.push(path.clone());
            args.push(path.clone());
        }
    }

    // Conservative network posture: full host net ONLY for an allow-all policy
    // (allowed_domains non-empty == NetworkPolicy::Allowed). LoopbackOnly /
    // Disabled get a fresh network namespace (loopback-only, external blocked).
    // The socat domain-filter companion is deferred; a domain-allowlist policy
    // therefore gets no external egress (errs safe) until it lands.
    if policy.network.allowed_domains.is_empty() {
        args.push("--unshare-net".into());
    } else {
        args.push("--share-net".into());
    }

    let quoted = shell_escape_single(command);
    let joined = args.join(" ");
    let base = format!("bwrap {joined} -- /bin/sh -c {quoted}");
    if filesystem_disabled || policy.scrub_paths.is_empty() {
        return base;
    }
    // Host-side post-command scrub of planted bare-repo files (finding 4,
    // scrubBareGitRepoFiles in sandbox-adapter.ts:404). Runs OUTSIDE bwrap on
    // the host cwd after the command — captures bwrap's exit code immediately
    // (`rc=$?`), deletes the planted paths ENOENT-tolerantly (`rm -rf -- …
    // 2>/dev/null`), then restores the exit code (`exit "$rc"`). Empty list →
    // no suffix (byte-identical to the un-hardened string, handled above).
    let scrub_args = policy
        .scrub_paths
        .iter()
        .map(|p| shell_escape_single(p))
        .collect::<Vec<_>>()
        .join(" ");
    format!("{base}\nrc=$?; rm -rf -- {scrub_args} 2>/dev/null; exit \"$rc\"")
}

/// Single-quote-escape `s` for `/bin/sh -c`.
///
/// Returns `'<s with embedded single quotes escaped>'`. Embedded `'` is
/// replaced with `'\''` (close quote, escaped quote, reopen quote).
fn shell_escape_single(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str(r"'\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

// =============================================================================
// macOS — SBPL profile
// =============================================================================

fn wrap_macos_sbpl(
    command: &str,
    policy: &SandboxRuntimeConfig,
) -> Result<String, SandboxWrapError> {
    // Derive a per-command log tag (claude-code `R0d`/`generateLogTag`) so the
    // `log stream` violation monitor can correlate this run's denials — matching
    // claude-code semantics (and what the live `sandbox-runtime` runner already
    // does). The profile content is now byte-identical to claude-code's k0d; only
    // the invocation form stays Generator-A-shaped (`sandbox-exec -f <tempfile>`).
    let log_tag = sandbox_runtime::macos::generate_log_tag(command);
    let profile = generate_sbpl_profile_with(policy, &log_tag);
    let profile_path = write_sbpl_tempfile(&profile)?;
    let quoted = shell_escape_single(command);
    Ok(format!(
        "sandbox-exec -f {profile_path} /bin/sh -c {quoted}",
    ))
}

/// Generate the macOS SBPL profile, byte-identical to claude-code's profile
/// builder (`k0d`/`generateSandboxProfile`). Rather than duplicate the ~300-line
/// template here, this delegates to the already byte-faithful, unit-tested
/// builder in `sandbox-runtime` ([`generate_sandbox_profile`]); this function is
/// the *config bridge* that maps this crate's [`SandboxRuntimeConfig`] onto the
/// builder's [`ProfileParams`].
///
/// `log_tag` is interpolated into `(deny default (with message "<tag>"))` and
/// every rule's `(with message …)` (the builder's `m`). Callers without a real
/// command-derived tag pass [`DEFAULT_SBPL_LOG_TAG`] (see [`generate_sbpl_profile`]).
pub(crate) fn generate_sbpl_profile_with(policy: &SandboxRuntimeConfig, log_tag: &str) -> String {
    let read_config = build_read_config(policy);
    let write_config = build_write_config(policy);
    generate_sandbox_profile(&ProfileParams {
        read_config: read_config.as_ref(),
        write_config: write_config.as_ref(),
        http_proxy_port: policy.network.http_proxy_port,
        socks_proxy_port: policy.network.socks_proxy_port,
        needs_network_restriction: needs_network_restriction(policy),
        allow_unix_sockets: opt_slice(&policy.network.allow_unix_sockets),
        allow_all_unix_sockets: policy.network.allow_all_unix_sockets,
        allow_local_binding: policy.network.allow_local_binding,
        allow_mach_lookup: opt_slice(&policy.network.allow_mach_lookup),
        allow_pty: policy.allow_pty,
        allow_git_config: policy.filesystem.allow_git_config,
        enable_weaker_network_isolation: policy.enable_weaker_network_isolation,
        allow_apple_events: policy.allow_apple_events,
        log_tag,
    })
}

/// Back-compat shim: the old 1-arg signature, defaulting to the canonical static
/// log tag (used by tests / any caller that has no command-derived tag).
#[cfg(test)]
pub(crate) fn generate_sbpl_profile(policy: &SandboxRuntimeConfig) -> String {
    generate_sbpl_profile_with(policy, DEFAULT_SBPL_LOG_TAG)
}

/// The log tag used when there is no command-derived tag. claude-code's
/// `generateSandboxProfile` interpolates the tag raw; an empty tag yields the
/// canonical `(deny default (with message ""))` form.
#[cfg(test)]
const DEFAULT_SBPL_LOG_TAG: &str = "";

/// `removeTrailingGlobSuffix` (`zhe`) normalisation the binary's upstream config
/// builder applies to every user-supplied read/write path before handing it to
/// the profile builder.
fn normalize(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .map(|p| remove_trailing_glob_suffix(p))
        .collect()
}

/// Map this crate's read restrictions onto the builder's [`ReadConfig`]
/// (`denyOnly` / `allowWithinDeny`), normalising each path as the binary's `W2i`
/// does. `None` ⇒ the builder emits the bare `(allow file-read*)` — emitted only
/// when there is nothing to deny/re-allow (byte-identical to a `Some` with empty
/// vectors, which `v0d` also renders as just `(allow file-read*)`).
fn build_read_config(policy: &SandboxRuntimeConfig) -> Option<ReadConfig> {
    if policy.filesystem.disabled {
        return None;
    }
    if policy.filesystem.deny_read.is_empty() && policy.filesystem.allow_read.is_empty() {
        return None;
    }
    Some(ReadConfig {
        deny_only: normalize(&policy.filesystem.deny_read),
        allow_within_deny: normalize(&policy.filesystem.allow_read),
    })
}

/// Map this crate's write restrictions onto the builder's [`WriteConfig`]. The
/// binary's `W2i` ALWAYS produces a defined writeConfig with `allowOnly =
/// [...get_default_write_paths(), ...allowWrite]` — the base set
/// (`/dev/stdout`, `/dev/null`, `/tmp/claude`, …) is unconditionally writable so
/// ordinary shell commands work, and writes are otherwise restricted to the
/// allow list (NOT wide open). We therefore ALWAYS return `Some` (never the bare
/// `(allow file-write*)` fallback) and prepend the base paths, matching the
/// faithful `sandbox-runtime` runner (`manager::build_fs_configs_macos`).
fn build_write_config(policy: &SandboxRuntimeConfig) -> Option<WriteConfig> {
    if policy.filesystem.disabled {
        return None;
    }
    let mut allow_only = get_default_write_paths();
    allow_only.extend(normalize(&policy.filesystem.allow_write));
    Some(WriteConfig {
        allow_only,
        deny_within_allow: normalize(&policy.filesystem.deny_write),
    })
}

/// The builder's `needsNetworkRestriction`. The binary restricts whenever an
/// `allowedDomains` allow-list is configured (then enforces it via the filtering
/// proxy). This crate uses the wildcard `"*"` as its "no restriction" sentinel
/// (matching the bwrap path), so full network (`(allow network*)`) is emitted
/// ONLY for a wildcard-all policy; a specific allow-list (or empty) restricts —
/// a non-wildcard list must never silently grant full egress.
///
/// NOTE: the legacy `sandbox-exec -f` path has no filtering proxy, so a specific
/// allow-list yields the restricted branch (no general egress) rather than
/// proxy-enforced domain filtering — the proxy model lives in the
/// `sandbox-runtime` live runner. This is the safe (more-restrictive) direction.
fn needs_network_restriction(policy: &SandboxRuntimeConfig) -> bool {
    !policy.network.allowed_domains.iter().any(|d| d == "*")
}

/// `&[]` ⇒ `None` — the builder omits the `allowMachLookup` / `allowUnixSockets`
/// sections on an empty/undefined list (its `l && l.length > 0` guards), rather
/// than emitting an empty section.
fn opt_slice(v: &[String]) -> Option<&[String]> {
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

fn write_sbpl_tempfile(profile: &str) -> Result<String, SandboxWrapError> {
    use std::io::Write;
    let mut f = tempfile::Builder::new()
        .prefix("lingxi-sandbox-")
        .suffix(".sb")
        .tempfile()
        .map_err(|e| SandboxWrapError::SbplWrite(e.to_string()))?;
    f.write_all(profile.as_bytes())
        .map_err(|e| SandboxWrapError::SbplWrite(e.to_string()))?;
    // Persist: the tempfile must outlive this function so `sandbox-exec -f`
    // can read it. The OS will clean it up at reboot; for long-lived
    // processes the platform layer is responsible for explicit unlink.
    let (_file, path) = f
        .keep()
        .map_err(|e| SandboxWrapError::SbplWrite(e.error.to_string()))?;
    Ok(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::{generate_sbpl_profile, generate_sbpl_profile_with, wrap_linux_bwrap};
    use crate::runtime_config::SandboxRuntimeConfig;

    // ── k0d-parity bridge tests: pin the section boundaries the config→params
    // mapping controls. The exhaustive per-rule byte-pinning lives in
    // `sandbox-runtime`'s `macos` tests; these prove SandboxRuntimeConfig maps
    // onto the k0d builder faithfully. All use a fixed log tag for determinism.

    #[test]
    fn sbpl_header_block_is_byte_exact() {
        let p = generate_sbpl_profile_with(&SandboxRuntimeConfig::default(), "TAG");
        assert!(
            p.starts_with(
                "(version 1)\n(deny default (with message \"TAG\"))\n\n; LogTag: TAG\n\n\
                 ; Essential permissions - based on Chrome sandbox policy\n\
                 ; Process permissions\n(allow process-exec)\n(allow process-fork)\n\
                 (allow process-info* (target same-sandbox))\n\
                 (allow signal (target same-sandbox))\n\
                 (allow mach-priv-task-port (target same-sandbox))"
            ),
            "header mismatch:\n{p}"
        );
    }

    #[test]
    fn sbpl_mach_allowlist_is_present() {
        let p = generate_sbpl_profile_with(&SandboxRuntimeConfig::default(), "TAG");
        assert!(
            p.contains("(allow mach-lookup\n  (global-name \"com.apple.audio.systemsoundserver\")")
        );
        assert!(p.contains("(global-name \"com.apple.coreservices.launchservicesd\")\n)"));
    }

    #[test]
    fn sbpl_network_unrestricted_when_domains_present() {
        let mut cfg = SandboxRuntimeConfig::default();
        cfg.network.allowed_domains = vec!["*".into()];
        let p = generate_sbpl_profile_with(&cfg, "TAG");
        assert!(p.contains("; Network\n(allow network*)\n"), "{p}");
    }

    #[test]
    fn sbpl_network_restricted_proxy_and_local_binding() {
        let mut cfg = SandboxRuntimeConfig::default(); // domains empty ⇒ restricted
        cfg.network.allow_local_binding = true;
        cfg.network.http_proxy_port = Some(8080);
        let p = generate_sbpl_profile_with(&cfg, "TAG");
        assert!(!p.contains("(allow network*)"), "{p}");
        assert!(p.contains("(allow network-bind (local ip \"*:*\"))"), "{p}");
        assert!(
            p.contains("(allow network-bind (local ip \"localhost:8080\"))"),
            "{p}"
        );
        assert!(
            p.contains("(allow network-outbound (remote ip \"localhost:8080\"))"),
            "{p}"
        );
    }

    #[test]
    fn sbpl_file_read_default_allow_all() {
        let p = generate_sbpl_profile_with(&SandboxRuntimeConfig::default(), "TAG");
        assert!(p.contains("; File read\n(allow file-read*)\n"), "{p}");
    }

    #[test]
    fn sbpl_file_read_deny_then_reallow_in_order() {
        let mut cfg = SandboxRuntimeConfig::default();
        cfg.filesystem.deny_read = vec!["/x".into()];
        cfg.filesystem.allow_read = vec!["/x/y".into()];
        let p = generate_sbpl_profile_with(&cfg, "TAG");
        let deny = p
            .find("(deny file-read*\n  (subpath \"/x\")")
            .expect("deny present");
        let allow = p
            .find("(allow file-read*\n  (subpath \"/x/y\")")
            .expect("re-allow present");
        assert!(deny < allow, "deny must precede re-allow");
        // directory-metadata allow appears once a read deny exists (k0d).
        assert!(
            p.contains("(allow file-read-metadata\n  (vnode-type DIRECTORY))"),
            "{p}"
        );
    }

    #[test]
    fn sbpl_file_write_base_paths_then_allowonly_then_mandatory_denies() {
        let mut cfg = SandboxRuntimeConfig::default();
        cfg.filesystem.allow_write = vec!["/work".into()];
        let p = generate_sbpl_profile_with(&cfg, "TAG");
        // The base writable set (claude-code TLt) is ALWAYS prepended so ordinary
        // shell I/O works — `/dev/null`, `/dev/stdout`, `/tmp/claude`, …
        assert!(
            p.contains("(allow file-write*\n  (subpath \"/dev/null\")"),
            "{p}"
        );
        assert!(
            p.contains("(allow file-write*\n  (subpath \"/dev/stdout\")"),
            "{p}"
        );
        // The user allow path is present too.
        assert!(
            p.contains("(allow file-write*\n  (subpath \"/work\")"),
            "{p}"
        );
        // mandatory git-config write-deny present by default; move-blocking pairs.
        assert!(
            p.contains(".git/config"),
            "git-config must be denied by default:\n{p}"
        );
        assert!(p.contains("(deny file-write-unlink"), "{p}");
        assert!(p.contains("(deny file-write-create"), "{p}");
    }

    #[test]
    fn sbpl_default_config_is_not_write_open() {
        // REGRESSION GUARD: an empty/default config must NOT yield bare
        // `(allow file-write*)` (writes everywhere). It restricts to the base set.
        let p = generate_sbpl_profile_with(&SandboxRuntimeConfig::default(), "TAG");
        assert!(
            !p.contains("; File write\n(allow file-write*)\n"),
            "default must not be write-open:\n{p}"
        );
        assert!(
            p.contains("(allow file-write*\n  (subpath \"/dev/null\")"),
            "{p}"
        );
    }

    #[test]
    fn filesystem_disabled_makes_sbpl_read_write_open() {
        let mut cfg = SandboxRuntimeConfig::default();
        cfg.filesystem.disabled = true;
        cfg.filesystem.allow_write = vec!["/work".into()];
        cfg.filesystem.deny_write = vec!["/work/.git".into()];
        cfg.filesystem.deny_read = vec!["/secret".into()];
        let p = generate_sbpl_profile_with(&cfg, "TAG");
        assert!(p.contains("; File read\n(allow file-read*)\n"), "{p}");
        assert!(p.contains("(allow file-write*)"), "{p}");
        assert!(!p.contains(".git/config"), "{p}");
    }

    #[test]
    fn sbpl_allow_git_config_drops_the_config_deny() {
        let mut deny = SandboxRuntimeConfig::default();
        deny.filesystem.allow_write = vec!["/work".into()];
        let mut allow = deny.clone();
        allow.filesystem.allow_git_config = true;
        assert!(generate_sbpl_profile_with(&deny, "TAG").contains(".git/config"));
        assert!(!generate_sbpl_profile_with(&allow, "TAG").contains(".git/config"));
    }

    #[test]
    fn sbpl_pty_block_only_when_enabled() {
        let mut cfg = SandboxRuntimeConfig::default();
        assert!(!generate_sbpl_profile_with(&cfg, "TAG").contains("pseudo-tty"));
        cfg.allow_pty = true;
        let p = generate_sbpl_profile_with(&cfg, "TAG");
        assert!(p.contains("(allow pseudo-tty)"), "{p}");
        assert!(p.contains("(literal \"/dev/ptmx\")"), "{p}");
    }

    #[test]
    fn sbpl_user_mach_lookup_prefix_and_exact() {
        let mut cfg = SandboxRuntimeConfig::default();
        cfg.network.allow_mach_lookup = vec!["com.foo.bar".into(), "com.foo.*".into()];
        let p = generate_sbpl_profile_with(&cfg, "TAG");
        assert!(
            p.contains("(allow mach-lookup (global-name \"com.foo.bar\"))"),
            "{p}"
        );
        assert!(
            p.contains("(allow mach-lookup (global-name-prefix \"com.foo.\"))"),
            "{p}"
        );
    }

    #[test]
    fn macos_sbpl_loopback_does_not_grant_full_network() {
        // finding-2 analog on the macOS backend: a LoopbackOnly policy
        // (allow_local_binding true, allowed_domains empty) must NOT emit the
        // full `(allow network*)` egress rule.
        let mut cfg = SandboxRuntimeConfig::default();
        cfg.network.allow_local_binding = true;
        cfg.network.allow_all_unix_sockets = true;
        assert!(
            !generate_sbpl_profile(&cfg).contains("(allow network*)"),
            "loopback/unix-socket policy must not grant full macOS network egress"
        );
        // Only a full-allow policy (allowed_domains non-empty) gets full network.
        cfg.network.allowed_domains = vec!["*".into()];
        assert!(generate_sbpl_profile(&cfg).contains("(allow network*)"));
    }

    #[test]
    fn bwrap_creates_a_user_namespace() {
        let w = wrap_linux_bwrap("true", &SandboxRuntimeConfig::default());
        assert!(
            w.contains("--unshare-user-try"),
            "bwrap must request a userns (degrading) so it can create pid/net ns unprivileged: {w}"
        );
    }

    #[test]
    fn share_net_only_when_allowed_domains_present() {
        let mut cfg = SandboxRuntimeConfig::default();
        // Allowed → ["*"] → --share-net
        cfg.network.allowed_domains = vec!["*".into()];
        assert!(wrap_linux_bwrap("true", &cfg).contains("--share-net"));
        // LoopbackOnly: empty domains + allow_local_binding → NOT --share-net
        cfg.network.allowed_domains.clear();
        cfg.network.allow_local_binding = true;
        let w = wrap_linux_bwrap("true", &cfg);
        assert!(
            !w.contains("--share-net"),
            "loopback must not get full egress: {w}"
        );
        assert!(w.contains("--unshare-net"));
    }

    #[test]
    fn ro_bind_in_place_comes_after_allow_write_so_deny_wins() {
        let mut cfg = SandboxRuntimeConfig::default();
        cfg.filesystem.allow_write = vec!["/work".into()];
        cfg.ro_bind_in_place = vec!["/work/.git/HEAD".into()];
        let w = wrap_linux_bwrap("true", &cfg);
        let bind_pos = w.find("--bind /work /work").expect("allow_write bind");
        let ro_pos = w
            .find("--ro-bind /work/.git/HEAD /work/.git/HEAD")
            .expect("ro-bind-in-place");
        assert!(
            ro_pos > bind_pos,
            "ro-bind-in-place must follow allow_write bind to override it:\n{w}"
        );
    }

    #[test]
    fn filesystem_disabled_skips_extra_binds_and_scrub_suffix() {
        let mut cfg = SandboxRuntimeConfig::default();
        cfg.filesystem.disabled = true;
        cfg.filesystem.allow_write = vec!["/work".into()];
        cfg.ro_bind_in_place = vec!["/work/.git/HEAD".into()];
        cfg.scrub_paths = vec!["/work/.git/HEAD".into()];
        let w = wrap_linux_bwrap("true", &cfg);
        assert!(w.contains("--bind / /"), "{w}");
        assert!(!w.contains("--ro-bind / /"), "{w}");
        assert!(!w.contains("--bind /work /work"), "{w}");
        assert!(
            !w.contains("--ro-bind /work/.git/HEAD /work/.git/HEAD"),
            "{w}"
        );
        assert!(!w.contains("rm -rf --"), "{w}");
    }

    #[test]
    fn scrub_paths_append_exit_preserving_host_side_rm() {
        let cfg = SandboxRuntimeConfig {
            // includes a quote to test escaping
            scrub_paths: vec!["/s/HEAD".into(), "/s/ob'j".into()],
            ..Default::default()
        };
        let w = wrap_linux_bwrap("true", &cfg);
        assert!(w.contains("rc=$?"), "must capture bwrap exit: {w}");
        assert!(
            w.contains("exit \"$rc\"") || w.contains("exit $rc"),
            "must restore exit code: {w}"
        );
        assert!(w.contains("rm -rf --"), "must rm the scrub paths: {w}");
        assert!(
            w.contains(r"'/s/ob'\''j'"),
            "scrub paths single-quote escaped: {w}"
        );
    }

    #[test]
    fn no_scrub_suffix_when_list_empty() {
        let w = wrap_linux_bwrap("true", &SandboxRuntimeConfig::default());
        assert!(
            !w.contains("rc=$?"),
            "empty scrub list must not append a suffix (byte-identical): {w}"
        );
    }
}
