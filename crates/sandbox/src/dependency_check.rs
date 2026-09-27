//! Sandbox dependency probing + the `sandbox_unavailable_reason` decoder.
//!
//! Port of `checkDependencies` and `getSandboxUnavailableReason` from
//! `claude-code/src/utils/sandbox/sandbox-adapter.ts`.
//!
//! All error strings are byte-for-byte from the claude-code source. See the
//! "1:1 fidelity items" in the plan document.

use crate::runtime_config::Platform;
use std::path::PathBuf;

/// `SandboxDependencyCheck` mirrors claude-code's TS type. `errors` non-empty
/// means the sandbox cannot run; `warnings` is informational only.
#[derive(Debug, Clone, Default)]
pub struct SandboxDependencyCheck {
    /// One human-readable string per missing or broken dependency.
    pub errors: Vec<String>,
    /// One human-readable string per non-fatal issue.
    pub warnings: Vec<String>,
    /// `true` iff the current platform is in `sandbox.enabledPlatforms` (or
    /// the list is unset, which is treated as "all enabled").
    pub in_enabled_list: bool,
}

/// Per-platform missing-deps summary. Useful for `/doctor` output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MissingDeps {
    /// macOS-only: `/usr/bin/sandbox-exec` missing from `$PATH`.
    pub sandbox_exec: bool,
    /// Linux/WSL2: `bwrap` missing.
    pub bwrap: bool,
    /// Linux/WSL2: `socat` missing.
    pub socat: bool,
}

impl MissingDeps {
    /// Aggregate into the public [`SandboxDependencyCheck::errors`] format.
    /// Each missing tool produces one entry in `errors`.
    #[must_use]
    pub fn into_errors(self) -> Vec<String> {
        let mut errors = Vec::new();
        if self.sandbox_exec {
            errors.push("sandbox-exec not found".to_string());
        }
        if self.bwrap {
            errors.push("bwrap not found".to_string());
        }
        // socat is NOT a blocking error: the conservative network posture does
        // not shell to socat (a non-full-allow policy maps to --unshare-net, no
        // proxy). A missing socat must NEVER disable the sandbox (finding 1) —
        // it only limits the (deferred) domain-filter companion. Surfaced as a
        // warning by `check_dependencies` instead.
        errors
    }
}

/// Probe required dependencies for the given platform.
///
/// `in_enabled_list` is supplied by the caller (the platform crate is what
/// knows the `enabledPlatforms` setting; this crate stays OS-agnostic).
#[must_use]
pub fn check_dependencies(
    platform: Option<Platform>,
    in_enabled_list: bool,
) -> SandboxDependencyCheck {
    let Some(platform) = platform else {
        return SandboxDependencyCheck {
            errors: vec!["platform not supported (requires macOS, Linux, or WSL2)".to_string()],
            warnings: vec![],
            in_enabled_list,
        };
    };

    let mut missing = MissingDeps::default();
    match platform {
        Platform::Mac => {
            missing.sandbox_exec = !which_exists("sandbox-exec");
        }
        Platform::Linux | Platform::Wsl => {
            missing.bwrap = !which_exists("bwrap");
            missing.socat = !which_exists("socat");
        }
    }

    let mut warnings = Vec::new();
    if missing.socat {
        warnings.push(
            "socat not found (domain-filtered networking unavailable; sandbox still enforced)"
                .to_string(),
        );
    }
    SandboxDependencyCheck {
        errors: missing.into_errors(),
        warnings,
        in_enabled_list,
    }
}

fn which_exists(cli: &str) -> bool {
    which::which(cli).is_ok()
}

/// Probe whether `cli` is on the host's `$PATH`. Convenience wrapper around
/// the `which` crate exposed for callers that want platform-specific probing
/// without going through `check_dependencies`.
#[must_use]
pub fn which_path(cli: &str) -> Option<PathBuf> {
    which::which(cli).ok()
}

/// Decide what (if any) human-readable reason to surface for sandbox being
/// unavailable. Returns `None` when no message is appropriate — either
/// sandbox is not enabled, or the sandbox can actually run.
///
/// Mirrors `getSandboxUnavailableReason` from sandbox-adapter.ts. The five
/// possible return strings are byte-for-byte from that source:
///
/// - `"sandbox.enabled is set but WSL1 is not supported (requires WSL2)"`
/// - `"sandbox.enabled is set but {platform} is not supported (requires macOS, Linux, or WSL2)"`
/// - `"sandbox.enabled is set but {platform} is not in sandbox.enabledPlatforms"`
/// - `"sandbox.enabled is set but dependencies are missing: {deps} · run /sandbox or /doctor for details"` (macOS)
/// - `"sandbox.enabled is set but dependencies are missing: {deps} · install missing tools (e.g. apt install bubblewrap socat) or run /sandbox for details"` (Linux/WSL)
///
/// Inputs:
/// - `enabled`: value of `sandbox.enabled` (returns `None` if false).
/// - `supported_platform`: whether the host runs a supported OS.
/// - `platform`: detected platform when supported; `None` when not.
/// - `wsl_one_detected`: whether `/proc/version` indicates WSL1.
/// - `raw_platform_label`: when `platform` is `None`, the unrecognized OS
///   name (e.g. `"windows"`, `"freebsd"`). Used only for the
///   "unsupported" string.
/// - `deps`: result of `check_dependencies()`.
#[must_use]
pub fn sandbox_unavailable_reason(
    enabled: bool,
    supported_platform: bool,
    platform: Option<Platform>,
    wsl_one_detected: bool,
    raw_platform_label: Option<String>,
    deps: &SandboxDependencyCheck,
) -> Option<String> {
    if !enabled {
        return None;
    }

    if wsl_one_detected {
        return Some(error_strings::WSL1_REFUSAL.to_string());
    }

    if !supported_platform {
        let label = raw_platform_label.unwrap_or_else(|| "unknown".to_string());
        return Some(error_strings::UNSUPPORTED_PLATFORM_TEMPLATE.replace("{platform}", &label));
    }

    if !deps.in_enabled_list {
        let label = platform.map_or("unknown", |p| p.as_str());
        return Some(error_strings::NOT_IN_ENABLED_PLATFORMS_TEMPLATE.replace("{platform}", label));
    }

    if !deps.errors.is_empty() {
        let joined = deps.errors.join(", ");
        let hint = match platform {
            Some(Platform::Mac) => error_strings::MISSING_DEPS_HINT_MAC,
            Some(Platform::Linux | Platform::Wsl) => error_strings::MISSING_DEPS_HINT_LINUX,
            None => "run /sandbox for details",
        };
        return Some(format!(
            "sandbox.enabled is set but dependencies are missing: {joined} · {hint}"
        ));
    }

    None
}

/// Byte-for-byte exact error strings from claude-code's sandbox-adapter.ts.
/// Tests assert against these to lock the messages in.
pub mod error_strings {
    /// WSL1 refusal — emitted when `/proc/version` indicates WSL1.
    pub const WSL1_REFUSAL: &str =
        "sandbox.enabled is set but WSL1 is not supported (requires WSL2)";

    /// Unsupported-platform template. `{platform}` is replaced with the OS
    /// label (e.g. `"windows"`, `"freebsd"`).
    pub const UNSUPPORTED_PLATFORM_TEMPLATE: &str =
        "sandbox.enabled is set but {platform} is not supported (requires macOS, Linux, or WSL2)";

    /// `enabledPlatforms` rejection template.
    pub const NOT_IN_ENABLED_PLATFORMS_TEMPLATE: &str =
        "sandbox.enabled is set but {platform} is not in sandbox.enabledPlatforms";

    /// Hint suffix for missing-deps on macOS.
    pub const MISSING_DEPS_HINT_MAC: &str = "run /sandbox or /doctor for details";

    /// Hint suffix for missing-deps on Linux/WSL.
    pub const MISSING_DEPS_HINT_LINUX: &str =
        "install missing tools (e.g. apt install bubblewrap socat) or run /sandbox for details";
}

#[cfg(test)]
mod tests {
    use super::MissingDeps;

    #[test]
    fn missing_socat_is_a_warning_not_a_blocking_error() {
        // `MissingDeps` is the real struct (plan referenced `MissingTools`);
        // same three pub fields.
        let missing = MissingDeps {
            sandbox_exec: false,
            bwrap: false,
            socat: true,
        };
        assert!(
            missing.into_errors().is_empty(),
            "socat must not block the sandbox"
        );
    }
}
