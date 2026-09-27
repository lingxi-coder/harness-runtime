//! Config schemas + validation. Ports the zod schemas of `sandbox-config.js`:
//! `domainPatternSchema`, `NetworkConfigSchema`, `FilesystemConfigSchema`,
//! `MitmProxyConfigSchema`, the `tlsTerminate` object, `RipgrepConfigSchema`,
//! `SeccompConfigSchema`, `WindowsConfigSchema`, `IgnoreViolationsConfigSchema`,
//! and the umbrella `SandboxRuntimeConfigSchema`. The `.refine()` rules are
//! ported into `validate()` methods (serde itself only handles shape/defaults).
//!
//! All `NetworkConfig` fields beyond `allowed_domains`/`denied_domains` are
//! additive: `Option` + `#[serde(default, skip_serializing_if=...)]`, so the
//! existing matcher/proxy code (which reads only the two domain lists) is
//! unaffected and a minimal `{allowedDomains, deniedDomains}` JSON still
//! round-trips identically.
//!
//! `filterRequest` (a runtime per-request callback in the TS schema) is NOT a
//! serializable value, so it is intentionally omitted from these serde structs.
//! The proxy already has its filter hook wired separately (P3b
//! `request_filter`); a host that wants per-request filtering supplies the
//! callback through that runtime seam, not through the config JSON.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::parent_proxy::ParentProxyConfig;

/// A live, swappable [`NetworkConfig`] handle shared between the
/// [`crate::manager::SandboxManager`] and the running proxies.
///
/// The TS keeps the network config in a module-level `let config` that the
/// running http/socks proxies read **per request** (via `filterNetworkRequest`),
/// so `updateConfig` is a **live swap**: changing the allow/deny lists takes
/// effect on the next connection with no proxy rebind. To reproduce that the
/// Rust proxies hold this shared handle and read the CURRENT inner
/// `Arc<NetworkConfig>` on every request; [`crate::manager::SandboxManager::update_config`]
/// writes the new network into it so already-running proxies see it immediately.
///
/// The outer [`RwLock`] is a **`std::sync::RwLock`**, deliberately. Each request
/// takes a read lock, clones the inner `Arc` to a local, and **drops the guard
/// BEFORE any `.await`** (see the proxy filter paths) — so the lock is never
/// held across a suspension point and cannot stall the async runtime.
pub type SharedNetworkConfig = Arc<RwLock<Arc<NetworkConfig>>>;

/// Build a [`SharedNetworkConfig`] from an owned [`NetworkConfig`] (the common
/// case: `manager.initialize` wraps `config.network`).
#[must_use]
pub fn shared_network_config(config: NetworkConfig) -> SharedNetworkConfig {
    Arc::new(RwLock::new(Arc::new(config)))
}

/// Read the CURRENT inner `Arc<NetworkConfig>` out of a [`SharedNetworkConfig`],
/// cloning the `Arc` (cheap) and **releasing the read guard before returning** —
/// so callers can `.await` on the result without holding the lock. A poisoned
/// lock (a writer panicked mid-swap) falls back to the poisoned inner value
/// rather than panicking the request path.
#[must_use]
pub fn current_network_config(shared: &SharedNetworkConfig) -> Arc<NetworkConfig> {
    match shared.read() {
        Ok(guard) => Arc::clone(&guard),
        Err(poisoned) => Arc::clone(&poisoned.into_inner()),
    }
}

/// A config-validation error carrying the (TS-faithful) message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigValidationError(pub String);

impl std::fmt::Display for ConfigValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigValidationError {}

/// Shorthand result for `validate()` methods.
type ValidateResult = Result<(), ConfigValidationError>;

fn err(msg: impl Into<String>) -> ConfigValidationError {
    ConfigValidationError(msg.into())
}

/// Allow/deny domain lists plus the optional network knobs from
/// `NetworkConfigSchema` (`sandbox-config.js:93-175`). Empty `allowed_domains`
/// = DENY-ALL (the netns is unshared and no proxy sockets are bound) — NOT
/// allow-all (`sandbox-manager.js:590-599`).
///
/// Everything after `strict_allowlist` is additive and optional; the matcher
/// reads only the two domain lists plus `strict_allowlist`, so adding these
/// never changes its behavior.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkConfig {
    /// Hostname patterns permitted (deny-first, so a denied pattern still wins).
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    /// Hostname patterns always denied (checked before `allowed_domains`).
    #[serde(default)]
    pub denied_domains: Vec<String>,
    /// When `Some(true)`, an unmatched host is deterministically denied instead
    /// of consulting the ask callback (2.1.219 `strictAllowlist`; the filter's
    /// `if(!r||xl.network.strictAllowlist)` gate, 2.1.220 @229871903). The
    /// adapter sets the key only when true (`…||void 0`, @230074348), so the
    /// serialized shape omits it otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict_allowlist: Option<bool>,
    /// macOS only: Unix socket paths to allow. Ignored on Linux.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_unix_sockets: Option<Vec<String>>,
    /// If true, allow all Unix sockets (disables blocking on both platforms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_all_unix_sockets: Option<bool>,
    /// Whether to allow binding to local ports (default: false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_local_binding: Option<bool>,
    /// macOS only: additional XPC/Mach service names to allow looking up.
    /// Supports a single trailing-`*` prefix match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_mach_lookup: Option<Vec<String>>,
    /// Port of an external HTTP proxy to use instead of starting a local one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_proxy_port: Option<u16>,
    /// Port of an external SOCKS proxy to use instead of starting a local one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socks_proxy_port: Option<u16>,
    /// Optional MITM proxy: route matching domains through an upstream proxy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mitm_proxy: Option<MitmProxyConfig>,
    /// `[EXPERIMENTAL]` in-process TLS termination config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_terminate: Option<TlsTerminateConfig>,
    /// Upstream HTTP proxy for outbound connections (reuses P2's shape).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_proxy: Option<ParentProxyConfig>,
}

impl NetworkConfig {
    /// Validate the network sub-config (`NetworkConfigSchema`,
    /// `sandbox-config.js:93-175`): every domain in `allowed_domains` /
    /// `denied_domains` and in `mitm_proxy.domains` must be a valid pattern,
    /// `http_proxy_port`/`socks_proxy_port` are `≥1` (the `u16` type already
    /// caps at 65535; `0` is rejected here to mirror `.min(1)`), each
    /// `allow_mach_lookup` entry may use only a single trailing `*`, and the
    /// `mitm_proxy`/`tls_terminate` sub-validations run.
    ///
    /// # Errors
    /// Returns the first failing refine's TS-faithful message.
    pub fn validate(&self) -> ValidateResult {
        for d in self.allowed_domains.iter().chain(&self.denied_domains) {
            validate_domain_pattern(d)?;
        }
        if matches!(self.http_proxy_port, Some(0)) {
            return Err(err("Number must be greater than or equal to 1"));
        }
        if matches!(self.socks_proxy_port, Some(0)) {
            return Err(err("Number must be greater than or equal to 1"));
        }
        if let Some(machs) = &self.allow_mach_lookup {
            for m in machs {
                validate_mach_lookup_entry(m)?;
            }
        }
        if let Some(mitm) = &self.mitm_proxy {
            mitm.validate()?;
        }
        if let Some(tls) = &self.tls_terminate {
            tls.validate()?;
        }
        Ok(())
    }
}

/// `MitmProxyConfigSchema` (`sandbox-config.js:61-67`): route the listed
/// domains through an upstream MITM proxy over a Unix socket.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MitmProxyConfig {
    /// Unix socket path to the MITM proxy (non-empty).
    pub socket_path: String,
    /// Domains to route through the MITM proxy (non-empty; each a valid pattern).
    pub domains: Vec<String>,
}

impl MitmProxyConfig {
    /// Validate (`MitmProxyConfigSchema`): `socketPath` non-empty, `domains`
    /// non-empty and each a valid domain pattern.
    ///
    /// # Errors
    /// Empty socket path, empty domains list, or an invalid domain pattern.
    pub fn validate(&self) -> ValidateResult {
        if self.socket_path.is_empty() {
            return Err(err("String must contain at least 1 character(s)"));
        }
        if self.domains.is_empty() {
            return Err(err("Array must contain at least 1 element(s)"));
        }
        for d in &self.domains {
            validate_domain_pattern(d)?;
        }
        Ok(())
    }
}

/// The `tlsTerminate` object (`sandbox-config.js:147-170`). Provide a CA
/// cert+key, or omit both for an ephemeral CA.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TlsTerminateConfig {
    /// Path to a PEM-encoded CA certificate (non-empty if set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert_path: Option<String>,
    /// Path to the PEM-encoded private key for `ca_cert_path` (non-empty if set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_key_path: Option<String>,
}

impl TlsTerminateConfig {
    /// Validate (`.refine(o => !o.caCertPath === !o.caKeyPath)`): the cert and
    /// key paths must be provided together (both or neither).
    ///
    /// # Errors
    /// Exactly one of `ca_cert_path` / `ca_key_path` is set.
    pub fn validate(&self) -> ValidateResult {
        if self.ca_cert_path.is_some() != self.ca_key_path.is_some() {
            return Err(err("caCertPath and caKeyPath must be provided together"));
        }
        Ok(())
    }
}

/// `FilesystemConfigSchema` (`sandbox-config.js:179-196`) — the USER-facing
/// filesystem shape (the manager maps this to the derived `ReadConfig`/
/// `WriteConfig` of P4-2b). Each path is non-empty (`filesystemPathSchema`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemConfig {
    /// Paths denied for reading.
    #[serde(default)]
    pub deny_read: Vec<String>,
    /// Paths to re-allow reading within denied regions (precedence over
    /// `deny_read`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_read: Option<Vec<String>>,
    /// Paths allowed for writing.
    #[serde(default)]
    pub allow_write: Vec<String>,
    /// Paths denied for writing (precedence over `allow_write`).
    #[serde(default)]
    pub deny_write: Vec<String>,
    /// Allow writes to `.git/config` files (default: false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_git_config: Option<bool>,
}

impl FilesystemConfig {
    /// Validate (`filesystemPathSchema` = `.min(1)`): every path in every list
    /// must be non-empty.
    ///
    /// # Errors
    /// Any empty path string.
    pub fn validate(&self) -> ValidateResult {
        let lists = [
            Some(&self.deny_read),
            self.allow_read.as_ref(),
            Some(&self.allow_write),
            Some(&self.deny_write),
        ];
        for list in lists.into_iter().flatten() {
            for p in list {
                if p.is_empty() {
                    return Err(err("Path cannot be empty"));
                }
            }
        }
        Ok(())
    }
}

/// `RipgrepConfigSchema` (`sandbox-config.js:207-217`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RipgrepConfig {
    /// The ripgrep command to execute.
    pub command: String,
    /// Additional arguments to pass before ripgrep args.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// Override `argv[0]` when spawning (for multicall binaries).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv0: Option<String>,
}

/// `SeccompConfigSchema` (`sandbox-config.js:257-268`, Linux only).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeccompConfig {
    /// Path to the apply-seccomp binary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply_path: Option<String>,
    /// Invoke apply-seccomp as a multicall binary dispatching on `ARGV0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv0: Option<String>,
}

/// `WindowsConfigSchema` (`sandbox-config.js:223-253`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowsConfig {
    /// Discriminator group name (default `"sandbox-runtime-net"`). Ignored if
    /// `group_sid` is set.
    #[serde(default = "default_group_name")]
    pub group_name: String,
    /// Discriminator group SID (must match `^S-1-`). Overrides `group_name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_sid: Option<String>,
    /// WFP sublayer GUID (must be a UUID) under which filters were installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wfp_sublayer_guid: Option<String>,
    /// Inclusive `[low, high]` proxy port range (`lo<=hi && hi-lo<=64`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_port_range: Option<(u16, u16)>,
}

fn default_group_name() -> String {
    "sandbox-runtime-net".to_string()
}

impl Default for WindowsConfig {
    fn default() -> Self {
        Self {
            group_name: default_group_name(),
            group_sid: None,
            wfp_sublayer_guid: None,
            proxy_port_range: None,
        }
    }
}

impl WindowsConfig {
    /// Validate (`WindowsConfigSchema` refines): `group_sid` (if set) matches
    /// `^S-1-`, `wfp_sublayer_guid` (if set) is a UUID, and `proxy_port_range`
    /// (if set) satisfies `lo<=hi && hi-lo<=64`. The `group_name` `.min(1)`
    /// is also checked (defaulted to non-empty, but a JSON `""` is rejected).
    ///
    /// # Errors
    /// Bad SID prefix, non-UUID sublayer GUID, or an out-of-bounds port range.
    pub fn validate(&self) -> ValidateResult {
        if self.group_name.is_empty() {
            return Err(err("String must contain at least 1 character(s)"));
        }
        if let Some(sid) = &self.group_sid {
            if !sid.starts_with("S-1-") {
                return Err(err("must be an S-1-… SID string"));
            }
        }
        if let Some(guid) = &self.wfp_sublayer_guid {
            if !is_uuid(guid) {
                return Err(err("Invalid uuid"));
            }
        }
        if let Some((lo, hi)) = self.proxy_port_range {
            if lo < 1 {
                return Err(err("Number must be greater than or equal to 1"));
            }
            if lo > hi || hi - lo > 64 {
                return Err(err("low must be ≤ high and range width ≤ 64"));
            }
        }
        Ok(())
    }
}

/// `IgnoreViolationsConfigSchema` (`sandbox-config.js:201-203`): a map of
/// command patterns to filesystem paths to ignore violations for. `"*"`
/// matches all commands.
pub type IgnoreViolationsConfig = HashMap<String, Vec<String>>;

/// `SandboxRuntimeConfigSchema` (`sandbox-config.js:272-319`): the umbrella
/// config a consumer passes to the manager.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxRuntimeConfig {
    /// Network restrictions configuration.
    pub network: NetworkConfig,
    /// Filesystem restrictions configuration.
    pub filesystem: FilesystemConfig,
    /// Optional configuration for ignoring specific violations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_violations: Option<IgnoreViolationsConfig>,
    /// Enable weaker nested sandbox mode (for Docker environments).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_weaker_nested_sandbox: Option<bool>,
    /// Enable weaker network isolation (macOS `com.apple.trustd.agent`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_weaker_network_isolation: Option<bool>,
    /// Allow sending Apple Events / Launch Services open requests (macOS only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_apple_events: Option<bool>,
    /// Custom ripgrep configuration (default `{ command: "rg" }`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ripgrep: Option<RipgrepConfig>,
    /// Max directory depth to search for dangerous files on Linux (`1..=10`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mandatory_deny_search_depth: Option<u8>,
    /// Allow pseudo-terminal (pty) operations (macOS only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_pty: Option<bool>,
    /// Custom seccomp binary paths (Linux only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seccomp: Option<SeccompConfig>,
    /// Linux only: absolute path to the `bwrap` binary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bwrap_path: Option<String>,
    /// Linux only: absolute path to the `socat` binary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socat_path: Option<String>,
    /// Windows-specific settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows: Option<WindowsConfig>,
}

impl SandboxRuntimeConfig {
    /// Validate the whole config (`SandboxRuntimeConfigSchema`): runs every
    /// sub-validation plus the umbrella refines — `bwrap_path`/`socat_path`
    /// (if set) are absolute (`binaryPathSchema`), `mandatory_deny_search_depth`
    /// is in `1..=10`, and the network/filesystem/windows sub-configs validate.
    ///
    /// # Errors
    /// The first failing refine's TS-faithful message.
    pub fn validate(&self) -> ValidateResult {
        self.network.validate()?;
        self.filesystem.validate()?;
        if let Some(depth) = self.mandatory_deny_search_depth {
            if !(1..=10).contains(&depth) {
                return Err(err(if depth < 1 {
                    "Number must be greater than or equal to 1"
                } else {
                    "Number must be less than or equal to 10"
                }));
            }
        }
        for p in [&self.bwrap_path, &self.socat_path].into_iter().flatten() {
            validate_binary_path(p)?;
        }
        if let Some(win) = &self.windows {
            win.validate()?;
        }
        Ok(())
    }
}

/// `binaryPathSchema` (`sandbox-config.js:51-56`): non-empty + absolute.
///
/// # Errors
/// Empty or relative path.
fn validate_binary_path(p: &str) -> ValidateResult {
    if p.is_empty() {
        return Err(err("Path cannot be empty"));
    }
    if !Path::new(p).is_absolute() {
        return Err(err("Binary path must be absolute"));
    }
    Ok(())
}

/// `domainPatternSchema`'s refine as a fallible check (used by every list that
/// validates domains).
///
/// # Errors
/// Invalid domain pattern (carries the TS message).
fn validate_domain_pattern(val: &str) -> ValidateResult {
    if is_valid_domain_pattern(val) {
        Ok(())
    } else {
        Err(err(
            "Invalid domain pattern. Must be a valid domain (e.g., \"example.com\") or wildcard \
             (e.g., \"*.example.com\"). Overly broad patterns like \"*.com\" or \"*\" are not \
             allowed for security reasons.",
        ))
    }
}

/// `allowMachLookup`'s refine (`sandbox-config.js:113-118`): wildcards are only
/// allowed as a single trailing `*`. `val.endsWith('*') ? val.slice(0,-1) :
/// val` must not contain `*`.
///
/// # Errors
/// A `*` appears anywhere other than as a single trailing character.
fn validate_mach_lookup_entry(val: &str) -> ValidateResult {
    let prefix = val.strip_suffix('*').unwrap_or(val);
    if prefix.contains('*') {
        return Err(err(
            "Wildcards are only allowed as a single trailing \"*\" (e.g., \"com.example.*\" or \
             \"*\" for all services).",
        ));
    }
    Ok(())
}

/// Lightweight UUID check (`z.string().uuid()`) without a `uuid` dep:
/// canonical `8-4-4-4-12` lowercase/uppercase hex with dashes.
fn is_uuid(s: &str) -> bool {
    let groups = [8usize, 4, 4, 4, 12];
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == groups.len()
        && parts
            .iter()
            .zip(groups.iter())
            .all(|(p, &n)| p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Validate a domain pattern (`domainPatternSchema`, `sandbox-config.js:11-43`):
/// `localhost` | `*.dom.tld` (≥2 non-empty labels after `*.`) | exact `dom.tld`
/// (contains a dot, no leading/trailing dot). No protocol/path/port; no other
/// wildcard use.
#[must_use]
pub fn is_valid_domain_pattern(val: &str) -> bool {
    if val.contains("://") || val.contains('/') || val.contains(':') {
        return false;
    }
    if val == "localhost" {
        return true;
    }
    if let Some(domain) = val.strip_prefix("*.") {
        if !domain.contains('.') || domain.starts_with('.') || domain.ends_with('.') {
            return false;
        }
        let parts: Vec<&str> = domain.split('.').collect();
        return parts.len() >= 2 && parts.iter().all(|p| !p.is_empty());
    }
    if val.contains('*') {
        return false;
    }
    val.contains('.') && !val.starts_with('.') && !val.ends_with('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_grammar_matches_ts() {
        // sandbox-config.js:11-43 domainPatternSchema
        assert!(is_valid_domain_pattern("localhost"));
        assert!(is_valid_domain_pattern("example.com"));
        assert!(is_valid_domain_pattern("api.example.com"));
        assert!(is_valid_domain_pattern("*.example.com"));
        // rejected: protocol/path/port
        assert!(!is_valid_domain_pattern("https://example.com"));
        assert!(!is_valid_domain_pattern("example.com/path"));
        assert!(!is_valid_domain_pattern("example.com:443"));
        // rejected: too-broad wildcards
        assert!(!is_valid_domain_pattern("*.com"));
        assert!(!is_valid_domain_pattern("*"));
        assert!(!is_valid_domain_pattern("*."));
        assert!(!is_valid_domain_pattern("ex*ample.com"));
        // rejected: no dot / leading-trailing dot
        assert!(!is_valid_domain_pattern("nodot"));
        assert!(!is_valid_domain_pattern(".example.com"));
        assert!(!is_valid_domain_pattern("example.com."));
    }

    #[test]
    fn full_config_round_trips_camel_case() {
        let json = r#"{
            "network": {
                "allowedDomains": ["github.com", "*.npmjs.org"],
                "deniedDomains": ["evil.com"],
                "allowUnixSockets": ["/tmp/sock"],
                "allowAllUnixSockets": false,
                "allowLocalBinding": true,
                "allowMachLookup": ["com.apple.*", "exact.service"],
                "httpProxyPort": 8080,
                "socksProxyPort": 1080,
                "mitmProxy": { "socketPath": "/run/mitm.sock", "domains": ["api.example.com"] },
                "tlsTerminate": { "caCertPath": "/ca.pem", "caKeyPath": "/ca.key" },
                "parentProxy": { "http": "http://up:3128" }
            },
            "filesystem": {
                "denyRead": ["/secret"],
                "allowRead": ["/secret/ok"],
                "allowWrite": ["/work"],
                "denyWrite": ["/work/.git"],
                "allowGitConfig": true
            },
            "ignoreViolations": { "*": ["/var/log"] },
            "enableWeakerNestedSandbox": true,
            "enableWeakerNetworkIsolation": false,
            "allowAppleEvents": true,
            "ripgrep": { "command": "rg", "args": ["--hidden"], "argv0": "rg" },
            "mandatoryDenySearchDepth": 3,
            "allowPty": true,
            "seccomp": { "applyPath": "/bin/apply-seccomp", "argv0": "as" },
            "bwrapPath": "/usr/bin/bwrap",
            "socatPath": "/usr/bin/socat",
            "windows": {
                "groupName": "g",
                "groupSid": "S-1-5-32-544",
                "wfpSublayerGuid": "12345678-1234-1234-1234-123456789abc",
                "proxyPortRange": [60080, 60089]
            }
        }"#;
        let cfg: SandboxRuntimeConfig = serde_json::from_str(json).expect("parse");
        cfg.validate().expect("valid");
        // Round-trip back to JSON and re-parse: structurally stable.
        let back = serde_json::to_string(&cfg).expect("serialize");
        let cfg2: SandboxRuntimeConfig = serde_json::from_str(&back).expect("reparse");
        assert_eq!(
            cfg2.network.allowed_domains,
            vec!["github.com", "*.npmjs.org"]
        );
        assert_eq!(cfg2.network.http_proxy_port, Some(8080));
        assert_eq!(
            cfg2.network.mitm_proxy.as_ref().unwrap().socket_path,
            "/run/mitm.sock"
        );
        assert_eq!(cfg2.mandatory_deny_search_depth, Some(3));
        assert_eq!(
            cfg2.windows.as_ref().unwrap().group_sid.as_deref(),
            Some("S-1-5-32-544")
        );
        assert_eq!(
            cfg2.ignore_violations.unwrap().get("*").unwrap(),
            &vec!["/var/log".to_string()]
        );
    }

    #[test]
    fn minimal_network_config_is_additive() {
        // Only the two domain lists → the extension fields stay None and a
        // serialize drops them (skip_serializing_if), so the matcher/proxy
        // surface is byte-identical.
        let json = r#"{"allowedDomains":["a.com"],"deniedDomains":[]}"#;
        let nc: NetworkConfig = serde_json::from_str(json).expect("parse");
        assert!(nc.mitm_proxy.is_none() && nc.parent_proxy.is_none());
        let back = serde_json::to_string(&nc).expect("ser");
        assert_eq!(back, r#"{"allowedDomains":["a.com"],"deniedDomains":[]}"#);
    }

    #[test]
    fn strict_allowlist_serde_shape() {
        // Absent key parses to None (pre-2.1.219 configs stay valid)…
        let json = r#"{"allowedDomains":["a.com"],"deniedDomains":[]}"#;
        let nc: NetworkConfig = serde_json::from_str(json).expect("parse");
        assert!(nc.strict_allowlist.is_none());
        // …and only a set value serializes, camelCased, right after the domain
        // lists — matching the adapter's `strictAllowlist: …||void 0` shape.
        let json = r#"{"allowedDomains":["a.com"],"deniedDomains":[],"strictAllowlist":true}"#;
        let nc: NetworkConfig = serde_json::from_str(json).expect("parse");
        assert_eq!(nc.strict_allowlist, Some(true));
        let back = serde_json::to_string(&nc).expect("ser");
        assert_eq!(
            back,
            r#"{"allowedDomains":["a.com"],"deniedDomains":[],"strictAllowlist":true}"#
        );
    }

    #[test]
    fn mitm_proxy_validate() {
        // good
        MitmProxyConfig {
            socket_path: "/s".into(),
            domains: vec!["api.example.com".into()],
        }
        .validate()
        .expect("good");
        // empty socket
        assert!(MitmProxyConfig {
            socket_path: String::new(),
            domains: vec!["api.example.com".into()],
        }
        .validate()
        .is_err());
        // empty domains
        assert!(MitmProxyConfig {
            socket_path: "/s".into(),
            domains: vec![],
        }
        .validate()
        .is_err());
        // invalid domain pattern
        assert!(MitmProxyConfig {
            socket_path: "/s".into(),
            domains: vec!["*".into()],
        }
        .validate()
        .is_err());
    }

    #[test]
    fn tls_terminate_validate_together() {
        TlsTerminateConfig::default()
            .validate()
            .expect("neither ok");
        TlsTerminateConfig {
            ca_cert_path: Some("/c".into()),
            ca_key_path: Some("/k".into()),
        }
        .validate()
        .expect("both ok");
        let one = TlsTerminateConfig {
            ca_cert_path: Some("/c".into()),
            ca_key_path: None,
        };
        assert_eq!(
            one.validate().unwrap_err().to_string(),
            "caCertPath and caKeyPath must be provided together"
        );
    }

    #[test]
    fn windows_config_defaults_and_validate() {
        assert_eq!(WindowsConfig::default().group_name, "sandbox-runtime-net");
        // group_name default when absent in JSON
        let w: WindowsConfig = serde_json::from_str("{}").expect("parse");
        assert_eq!(w.group_name, "sandbox-runtime-net");
        w.validate().expect("default valid");
        // bad SID
        assert!(WindowsConfig {
            group_sid: Some("X-1-5".into()),
            ..Default::default()
        }
        .validate()
        .is_err());
        // bad UUID
        assert!(WindowsConfig {
            wfp_sublayer_guid: Some("not-a-uuid".into()),
            ..Default::default()
        }
        .validate()
        .is_err());
        // good UUID
        WindowsConfig {
            wfp_sublayer_guid: Some("12345678-1234-1234-1234-123456789abc".into()),
            ..Default::default()
        }
        .validate()
        .expect("uuid ok");
        // range lo>hi
        assert!(WindowsConfig {
            proxy_port_range: Some((100, 90)),
            ..Default::default()
        }
        .validate()
        .is_err());
        // range too wide
        assert!(WindowsConfig {
            proxy_port_range: Some((100, 200)),
            ..Default::default()
        }
        .validate()
        .is_err());
        // good range
        WindowsConfig {
            proxy_port_range: Some((60080, 60089)),
            ..Default::default()
        }
        .validate()
        .expect("range ok");
    }

    #[test]
    fn sandbox_config_validate_refines() {
        let base = || SandboxRuntimeConfig {
            network: NetworkConfig {
                allowed_domains: vec!["a.com".into()],
                ..Default::default()
            },
            filesystem: FilesystemConfig::default(),
            ..Default::default()
        };
        base().validate().expect("base valid");
        // non-absolute bwrap_path
        let mut c = base();
        c.bwrap_path = Some("relative/bwrap".into());
        assert_eq!(
            c.validate().unwrap_err().to_string(),
            "Binary path must be absolute"
        );
        // depth out of range
        let mut c = base();
        c.mandatory_deny_search_depth = Some(11);
        assert!(c.validate().is_err());
        let mut c = base();
        c.mandatory_deny_search_depth = Some(0);
        assert!(c.validate().is_err());
        // bad network domain pattern
        let mut c = base();
        c.network.allowed_domains = vec!["*.com".into()];
        assert!(c.validate().is_err());
        // bad mach lookup wildcard (non-trailing)
        let mut c = base();
        c.network.allow_mach_lookup = Some(vec!["com.*.example".into()]);
        assert!(c.validate().is_err());
        // good mach lookup (trailing * and exact)
        let mut c = base();
        c.network.allow_mach_lookup =
            Some(vec!["com.example.*".into(), "exact".into(), "*".into()]);
        c.validate().expect("mach ok");
    }

    #[test]
    fn filesystem_validate_rejects_empty_path() {
        let fs = FilesystemConfig {
            deny_read: vec![String::new()],
            ..Default::default()
        };
        assert_eq!(
            fs.validate().unwrap_err().to_string(),
            "Path cannot be empty"
        );
        FilesystemConfig {
            deny_read: vec!["/x".into()],
            allow_write: vec!["/y".into()],
            ..Default::default()
        }
        .validate()
        .expect("ok");
    }
}
