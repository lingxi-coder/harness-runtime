//! Wire-shape `SandboxRuntimeConfig` matching claude-code's zod
//! `SandboxSettingsSchema` from `entrypoints/sandboxTypes.ts`.
//!
//! Every `#[serde(rename_all = "camelCase")]` here lines up with a zod field
//! name byte-for-byte. Renaming any field is a managed-policy-breaking change
//! and must be coordinated with claude-code's settings schema.

use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;

/// Platforms the sandbox runtime recognizes.
///
/// Mirrors claude-code's `Platform` literal union (`macos` | `linux` | `wsl`).
/// `windows` is intentionally absent — claude-code refuses sandbox on Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Platform {
    /// macOS host (Darwin) — sandbox via `sandbox-exec`.
    /// Wire spelling is `"macos"` to match claude-code `getPlatform()`
    /// (platform.ts:14) and the `enabledPlatforms: ["macos"]` example
    /// (sandbox-adapter.ts:503). `alias = "mac"` accepts the older spelling
    /// on deserialize for back-compat.
    #[serde(rename = "macos", alias = "mac")]
    Mac,
    /// Native Linux host — sandbox via `bwrap`.
    #[serde(rename = "linux")]
    Linux,
    /// WSL2 host — same `bwrap` path as native Linux, distinguished for
    /// claude-code parity (WSL1 is refused before sandbox dispatch).
    #[serde(rename = "wsl")]
    Wsl,
}

/// Deserialize `Option<Vec<Platform>>` LENIENTLY: a list element that is not a
/// recognized platform (`"macos"`/`"mac"`, `"linux"`, `"wsl"`) is DROPPED rather
/// than aborting the whole settings parse. This includes NON-STRING elements
/// (e.g. `[123]`): claude-code reads `enabledPlatforms` UNTYPED and only
/// `.includes()`-checks it (sandbox-adapter.ts:505-526), so a non-string entry
/// (a number, object, …) never matches any platform but never errors either.
/// We mirror that: a non-string element is treated exactly like an unknown
/// string — dropped and skipped. Without this leniency, the desktop tier
/// loader's `if let Ok(parsed)=from_str(..) else continue`
/// (engine-desktop/src/lib.rs:250,339) would silently DROP the ENTIRE tier (and
/// its `sandbox.enabled` / `failIfUnavailable`) on a single malformed element.
fn deserialize_enabled_platforms<'de, D>(deserializer: D) -> Result<Option<Vec<Platform>>, D::Error>
where
    D: Deserializer<'de>,
{
    // Accept absent → None, null → None, or an array (any element that is not a
    // recognized platform string is dropped — including non-string elements).
    // The top-level non-array shape still errors, matching zod's array shape
    // check, but individual element validation is lenient like claude-code's
    // untyped `.includes()`.
    let opt: Option<Vec<serde_json::Value>> = Option::deserialize(deserializer)?;
    let Some(raw) = opt else { return Ok(None) };
    let mut out = Vec::with_capacity(raw.len());
    for v in raw {
        // Reuse the enum's own deserialize for the spelling/alias rules. A
        // non-string element (or unknown string) fails this and is dropped,
        // never aborting the tier.
        if let Ok(p) = serde_json::from_value::<Platform>(v) {
            out.push(p);
        }
        // else: unknown platform string OR non-string element — drop it (lenient).
    }
    Ok(Some(out))
}

impl Platform {
    /// Human-readable form for inclusion in error strings.
    /// Matches claude-code's `getPlatform()` return value spelling
    /// (`macos`, `linux`, `wsl`).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Mac => "macos",
            Self::Linux => "linux",
            Self::Wsl => "wsl",
        }
    }
}

/// Network restriction subsection of `SandboxRuntimeConfig`.
///
/// All fields are passthrough from `SandboxNetworkConfigSchema` in
/// `entrypoints/sandboxTypes.ts`.
#[allow(clippy::struct_excessive_bools)] // wire-shape mirror; flags are independent and 1:1 with zod schema.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkRestrictionConfig {
    /// Domains the sandboxed process may reach outbound (egress allowlist).
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    /// Computed DENYLIST of domains the sandboxed process is refused outbound,
    /// checked BEFORE `allowed_domains` (sandbox-adapter.ts:179/212-220/362).
    /// Populated from `WebFetch(domain:...)` DENY rules. Honored from all
    /// sources even under `allow_managed_domains_only` — the comment on
    /// `allowManagedDomainsOnly` in sandboxTypes.ts:22-23 explicitly states
    /// "Denied domains are still respected from all sources."
    #[serde(default)]
    pub denied_domains: Vec<String>,
    /// When `true`, only `allowed_domains` from managed (enterprise) settings
    /// are honored; user-level rules are ignored. Mirrors zod
    /// `allowManagedDomainsOnly`.
    #[serde(default)]
    pub allow_managed_domains_only: bool,
    /// When `true`, the sandbox runtime DENIES a host that is not in
    /// `allowed_domains` instead of prompting (2.1.219 `strictAllowlist`).
    ///
    /// Oracle description, verbatim: "When true, the sandbox runtime
    /// deterministically denies hosts not in allowedDomains instead of
    /// prompting. Enforced for sandboxed commands only — in-process tools such
    /// as WebFetch are not gated by this setting. Only honored from user,
    /// managed/policy, or CLI (--settings) settings — project settings
    /// (.claude/settings.json and .claude/settings.local.json) are ignored."
    ///
    /// The source restriction is the load-bearing half and is NOT enforced
    /// here: this struct is the merged wire shape, so a project tier that set
    /// the key would land in it. The composition root resolves the honored
    /// value separately (`strict_allowlist_override`) exactly as it already
    /// does for `allowAppleEvents`.
    #[serde(default)]
    pub strict_allowlist: bool,
    /// Unix-socket paths the sandboxed process is permitted to connect to
    /// (e.g. `/var/run/docker.sock`).
    #[serde(default)]
    pub allow_unix_sockets: Vec<String>,
    /// When `true`, all unix sockets pass through (overrides
    /// `allow_unix_sockets`).
    #[serde(default)]
    pub allow_all_unix_sockets: bool,
    /// When `true`, the sandbox may bind to localhost (servers can listen
    /// on `127.0.0.1`).
    #[serde(default)]
    pub allow_local_binding: bool,
    /// Optional HTTP proxy port the sandbox-runtime listens on for HTTP
    /// CONNECT-style egress.
    #[serde(default)]
    pub http_proxy_port: Option<u16>,
    /// Optional SOCKS proxy port the sandbox-runtime listens on for
    /// SOCKS5 egress.
    #[serde(default)]
    pub socks_proxy_port: Option<u16>,
    /// User-specified XPC/Mach service names the sandbox may `mach-lookup`
    /// (claude-code k0d's `allowMachLookup`). A trailing `*` is emitted as a
    /// `global-name-prefix`. Empty ⇒ the "User-specified XPC/Mach services"
    /// section is omitted, so existing callers are byte-stable.
    #[serde(default)]
    pub allow_mach_lookup: Vec<String>,
}

/// Filesystem restriction subsection.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemRestrictionConfig {
    /// When `true`, disable filesystem sandboxing entirely while keeping the
    /// surrounding sandbox runtime active.
    #[serde(default)]
    pub disabled: bool,
    /// Paths the sandboxed process may write to (mounted read-write).
    #[serde(default)]
    pub allow_write: Vec<String>,
    /// Paths explicitly denied for writes even if otherwise within the
    /// writable tree (overrides `allow_write`).
    #[serde(default)]
    pub deny_write: Vec<String>,
    /// Paths explicitly denied for reads (overrides `allow_read`).
    #[serde(default)]
    pub deny_read: Vec<String>,
    /// Paths the sandboxed process may read (mounted read-only).
    #[serde(default)]
    pub allow_read: Vec<String>,
    /// When `true`, only `allow_read` paths from managed (enterprise)
    /// settings are honored. Mirrors zod `allowManagedReadPathsOnly`.
    #[serde(default)]
    pub allow_managed_read_paths_only: bool,
    /// When `true`, `.git/config` is NOT added to the mandatory write-deny list
    /// (claude-code k0d's `allowGitConfig`, default `false`). Default `false`
    /// ⇒ `.git/config` IS denied, matching claude-code's default.
    #[serde(default)]
    pub allow_git_config: bool,
}

/// Ripgrep override block. Bundled ripgrep path + extra args.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RipgrepConfig {
    /// Absolute path to the ripgrep executable. Empty means
    /// "use the bundled / `$PATH` default".
    #[serde(default)]
    pub command: String,
    /// Extra args appended after the user query (e.g. `--no-config`).
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional `argv0` override. In embedded mode (`argv0='rg'` dispatch),
    /// sandbox-runtime spawns the ripgrep process with this `argv0`
    /// (sandbox-adapter.ts:352-357). `RipgrepConfig` has NO `rename_all`;
    /// `argv0` is already lowercase so the wire key is `argv0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv0: Option<String>,
}

/// serde default for [`SandboxRuntimeConfig::allow_unsandboxed_commands`].
///
/// claude-code's zod schema documents `allowUnsandboxedCommands` as
/// `Default: true` (sandboxTypes.ts:113-120). An absent JSON key therefore
/// deserializes to `true`, NOT `false`.
fn default_true() -> bool {
    true
}

/// Full `SandboxRuntimeConfig` — direct port of zod `SandboxSettingsSchema`
/// (`entrypoints/sandboxTypes.ts`).
///
/// Field naming uses `#[serde(rename_all = "camelCase")]` so the wire shape
/// matches the zod schema byte-for-byte (`failIfUnavailable`,
/// `autoAllowBashIfSandboxed`, etc.). Unknown fields are silently dropped on
/// deserialization (zod's `.passthrough()` cannot be perfectly mirrored in
/// `serde` without an extra `HashMap<String, Value>` catch-all; we accept that
/// trade-off — managed settings always lay down only known fields).
#[allow(clippy::struct_excessive_bools)] // wire-shape mirror; the bool count comes from claude-code's zod schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxRuntimeConfig {
    /// Master toggle. When `false`, the sandbox runtime is bypassed.
    #[serde(default)]
    pub enabled: bool,
    /// When `true`, sandbox unavailability is fatal (errors propagate to the
    /// caller). When `false`, the runtime falls back to no-sandbox execution.
    #[serde(default)]
    pub fail_if_unavailable: bool,
    /// Environment variables the sandboxed process must NOT receive, from
    /// `sandbox.credentials.envVars` (see [`crate::credentials`]).
    ///
    /// ⚠️ Port-local. Upstream's srt config carries the whole `credentials`
    /// block because its proxy does the masking; this port only ever withholds,
    /// so the reduced form is the list of names to drop.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credential_deny_env: Vec<String>,

    /// Optional restriction of the platforms on which the sandbox runs. `None`
    /// means "run on every supported platform".
    #[serde(default, deserialize_with = "deserialize_enabled_platforms")]
    pub enabled_platforms: Option<Vec<Platform>>,
    /// When `true`, sandboxed bash invocations auto-approve permission
    /// prompts (the sandbox is the safety boundary, not the prompt).
    #[serde(default)]
    pub auto_allow_bash_if_sandboxed: bool,
    /// Allow commands to run outside the sandbox via the
    /// `dangerouslyDisableSandbox` parameter. When `false`, that parameter is
    /// completely ignored and all commands must run sandboxed. Mirrors zod
    /// `allowUnsandboxedCommands` (sandboxTypes.ts:113-120 /
    /// sandbox-adapter.ts:476). Default: `true` — an absent JSON key
    /// deserializes to `true`.
    #[serde(default = "default_true")]
    pub allow_unsandboxed_commands: bool,
    /// Network egress configuration.
    #[serde(default)]
    pub network: NetworkRestrictionConfig,
    /// Filesystem allow/deny configuration.
    #[serde(default)]
    pub filesystem: FilesystemRestrictionConfig,
    /// Map of violation-category → patterns whose sandbox violations are
    /// silently ignored (mostly noisy filesystem reads from build tools).
    #[serde(default)]
    pub ignore_violations: HashMap<String, Vec<String>>,
    /// When `true`, allow a sandbox-inside-sandbox configuration with weaker
    /// guarantees on the inner layer (claude-code feature flag).
    #[serde(default)]
    pub enable_weaker_nested_sandbox: bool,
    /// When `true`, relax network isolation (used for certain proxy stacks).
    #[serde(default)]
    pub enable_weaker_network_isolation: bool,
    /// When `true`, emit the pseudo-terminal (pty) block in the macOS profile
    /// (claude-code k0d's `allowPty`). Default `false` ⇒ no pty block.
    #[serde(default)]
    pub allow_pty: bool,
    /// When `true`, emit the Apple Events block (appleeventsd / Launch Services)
    /// in the macOS profile (claude-code k0d's `allowAppleEvents`). Default
    /// `false` ⇒ no block.
    #[serde(default)]
    pub allow_apple_events: bool,
    /// Commands that bypass the sandbox decision (e.g. `bazel`, `make`).
    #[serde(default)]
    pub excluded_commands: Vec<String>,
    /// Ripgrep override.
    #[serde(default)]
    pub ripgrep: RipgrepConfig,
    /// Host paths to mount read-only IN PLACE (`--ro-bind <p> <p>`), overriding
    /// any writable parent. Populated by the posix `prepare` layer from the
    /// EXISTING subset of denied + bare-repo paths (FS access lives there, not
    /// in the pure wrapper). claude-code denyWrite semantics
    /// (sandbox-adapter.ts:264). `#[serde(skip)]` keeps the TS-mirror wire shape
    /// byte-identical — this field is a host-side build artifact, not a managed
    /// setting.
    #[serde(skip)]
    pub ro_bind_in_place: Vec<String>,
    /// Host paths to delete AFTER the command (non-existent-at-config-time
    /// bare-repo files planted during the run). See finding 4 / Task 5.
    /// `#[serde(skip)]` for the same reason as [`Self::ro_bind_in_place`].
    #[serde(skip)]
    pub scrub_paths: Vec<String>,
}

impl Default for SandboxRuntimeConfig {
    /// Hand-written so the default is byte-equivalent to deserializing empty
    /// JSON (`{}`): every `#[serde(default)]` field gets its natural default,
    /// and `allow_unsandboxed_commands` gets `true` (the `default_true` serde
    /// helper). `SandboxRuntimeConfig::default() == from_value(json!({}))`.
    fn default() -> Self {
        Self {
            enabled: false,
            fail_if_unavailable: false,
            enabled_platforms: None,
            auto_allow_bash_if_sandboxed: false,
            allow_unsandboxed_commands: true,
            network: NetworkRestrictionConfig::default(),
            credential_deny_env: Vec::new(),
            filesystem: FilesystemRestrictionConfig::default(),
            ignore_violations: HashMap::new(),
            enable_weaker_nested_sandbox: false,
            enable_weaker_network_isolation: false,
            allow_pty: false,
            allow_apple_events: false,
            excluded_commands: Vec::new(),
            ripgrep: RipgrepConfig::default(),
            ro_bind_in_place: Vec::new(),
            scrub_paths: Vec::new(),
        }
    }
}

impl SandboxRuntimeConfig {
    /// THE canonical mapping for "may commands run unsandboxed via
    /// `dangerouslyDisableSandbox`?" — returns `allow_unsandboxed_commands`
    /// directly (sandbox-adapter.ts:476). Replaces the prior fabricated
    /// `!allow_unsandboxed_commands.is_empty()` reader.
    #[must_use]
    pub fn are_unsandboxed_commands_allowed(&self) -> bool {
        self.allow_unsandboxed_commands
    }
}

// ============================================================================
// SettingsJson slice — only the keys the sandbox conversion pipeline touches.
// Other claude-code SettingsJson fields are owned by other crates and may not
// even exist in lingxi-core (the conversion pipeline only cares about
// `permissions.allow/deny` and the `sandbox` subtree).
// ============================================================================

/// Slice of claude-code `SettingsJson` consumed by
/// [`crate::policy_convert::convert_settings_to_runtime_config`].
///
/// `#[serde(default)]` everywhere so partial JSON deserializes cleanly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsJson {
    /// `permissions.allow/deny` slice — drives filesystem and webfetch
    /// extraction.
    #[serde(default)]
    pub permissions: Option<SettingsPermissions>,
    /// User-supplied `sandbox` subsection (overrides extracted defaults).
    #[serde(default)]
    pub sandbox: Option<SandboxSettingsJson>,
    /// Directory the settings file lives in. Required because claude-code's
    /// path patterns (`/path`) are resolved relative to the settings file
    /// directory. Defaults to the cwd if the caller doesn't know it; sandbox
    /// path resolution then degrades to "current directory" semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings_dir: Option<std::path::PathBuf>,
}

/// `permissions` subsection of `SettingsJson`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsPermissions {
    /// Allow-list of `Tool(arg)` rules.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Deny-list of `Tool(arg)` rules.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Extra directories the caller wants mounted writable inside the
    /// sandbox (claude-code: `additionalDirectories`).
    #[serde(default)]
    pub additional_directories: Vec<String>,
}

/// User-supplied `sandbox` subsection of `SettingsJson`. All fields optional;
/// merged into [`SandboxRuntimeConfig`] by
/// [`crate::policy_convert::convert_settings_to_runtime_config`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxSettingsJson {
    /// Override `SandboxRuntimeConfig::enabled`.
    pub enabled: Option<bool>,
    /// Override `SandboxRuntimeConfig::fail_if_unavailable`.
    pub fail_if_unavailable: Option<bool>,
    /// Override `SandboxRuntimeConfig::enabled_platforms`.
    #[serde(default, deserialize_with = "deserialize_enabled_platforms")]
    pub enabled_platforms: Option<Vec<Platform>>,
    /// Override `SandboxRuntimeConfig::auto_allow_bash_if_sandboxed`.
    pub auto_allow_bash_if_sandboxed: Option<bool>,
    /// Override `SandboxRuntimeConfig::allow_unsandboxed_commands`.
    pub allow_unsandboxed_commands: Option<bool>,
    /// Override `SandboxRuntimeConfig::network` (merged with extracted
    /// `allowed_domains`).
    pub network: Option<NetworkRestrictionConfig>,
    /// Override `SandboxRuntimeConfig::filesystem` (merged with extracted
    /// allow/deny paths).
    pub filesystem: Option<FilesystemRestrictionConfig>,
    /// Override `SandboxRuntimeConfig::ignore_violations`.
    pub ignore_violations: Option<HashMap<String, Vec<String>>>,
    /// `sandbox.credentials` — credential files and environment variables to
    /// withhold from the sandboxed process.
    ///
    /// 🚨 Before this field existed the key was an UNKNOWN settings field:
    /// stripped and silently ignored, so a user who configured
    /// `{"mode": "mask"}` over their AWS keys got a sandbox that handed the real
    /// keys to every command. See [`crate::credentials`] for what this port
    /// enforces and what it degrades.
    pub credentials: Option<crate::credentials::SandboxCredentials>,
    /// Override `SandboxRuntimeConfig::enable_weaker_nested_sandbox`.
    pub enable_weaker_nested_sandbox: Option<bool>,
    /// Override `SandboxRuntimeConfig::enable_weaker_network_isolation`.
    pub enable_weaker_network_isolation: Option<bool>,
    // NB: there is intentionally NO `allow_pty` settings key. claude-code's
    // `SandboxSettingsSchema` has no `allowPty` field (2.1.207 @212793011 zod
    // schema / @212939769 settings-key allowlist both omit it), and its
    // sandbox-adapter never forwards one into the srt config — all 7 binary
    // `allowPty` occurrences are internal to the vendored sandbox-runtime package.
    // An `allowPty` key in a user settings.json is an UNKNOWN field: stripped and
    // never honored (matching CC). Do NOT re-add a settings override here — it
    // would be anti-parity. The engine `SandboxRuntimeConfig::allow_pty` field
    // stays for the legacy SBPL wrap path and the srt config schema, which
    // legitimately carry it.
    /// Override `SandboxRuntimeConfig::allow_apple_events`.
    ///
    /// SOURCE-RESTRICTED: claude-code honors `allowAppleEvents` ONLY from user,
    /// managed/policy, or CLI `--settings` (`flagSettings`) sources — project
    /// `.lingxi/settings.json` and `.lingxi/settings.local.json` are IGNORED
    /// (2.1.207 @223928133). This field is parsed by the composition root's
    /// per-source resolution, which threads the effective value via
    /// [`crate::policy_convert::SandboxConvertContext::allow_apple_events_override`];
    /// it is NOT applied from the merged settings blob in
    /// [`crate::policy_convert::convert_settings_to_runtime_config`].
    pub allow_apple_events: Option<bool>,
    /// Override `SandboxRuntimeConfig::excluded_commands`.
    pub excluded_commands: Option<Vec<String>>,
    /// Override `SandboxRuntimeConfig::ripgrep`.
    pub ripgrep: Option<RipgrepConfig>,
}

#[cfg(test)]
mod enabled_platforms_tests {
    use super::{Platform, SandboxRuntimeConfig};

    /// A NON-STRING element (`123`) in `enabledPlatforms` must be dropped
    /// leniently — NOT abort the whole settings tier. claude-code reads
    /// `enabledPlatforms` untyped and only `.includes()`-checks it, so a number
    /// never matches a platform but never errors. The recognized `"macos"`
    /// string survives, the tier parses, and `sandbox.enabled` is preserved.
    #[test]
    fn non_string_element_dropped_tier_survives() {
        let json = r#"{
            "enabled": true,
            "failIfUnavailable": true,
            "enabledPlatforms": [123, "macos"]
        }"#;
        let cfg: SandboxRuntimeConfig =
            serde_json::from_str(json).expect("malformed element must NOT abort the tier");
        assert!(cfg.enabled, "sandbox.enabled must survive a bad element");
        assert!(cfg.fail_if_unavailable);
        assert_eq!(
            cfg.enabled_platforms.as_deref(),
            Some(&[Platform::Mac][..]),
            "123 dropped, \"macos\" kept"
        );
    }

    /// Unknown platform STRINGS keep dropping leniently (regression guard).
    #[test]
    fn unknown_string_dropped() {
        let json = r#"{"enabledPlatforms": ["windows", "linux"]}"#;
        let cfg: SandboxRuntimeConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg.enabled_platforms.as_deref(),
            Some(&[Platform::Linux][..])
        );
    }
}
