//! Constants + service-name helpers shared between [`super::macos`] and
//! consumers of the macOS keychain backend (e.g. `keychain_prefetch`).
//!
//! 1:1 with claude-code's `macOsKeychainHelpers.ts`:
//! - [`full_service_name`] reproduces `getMacOsKeychainStorageServiceName`.
//! - [`compute_dir_hash`] reproduces the `sha256(configDir).hex()[..8]`
//!   prefix-only-when-non-default behavior.
//! - [`SECURITY_STDIN_LINE_LIMIT`] guards the `security -i` 4096-byte BUFSIZ.
//! - [`KEYCHAIN_CACHE_TTL`] reproduces the 30 s TTL.

use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;

/// Service-suffix appended to the legacy API-key service name to derive
/// the OAuth credentials entry. DO NOT change — part of the cross-version
/// keychain lookup key. Matches claude-code's `CREDENTIALS_SERVICE_SUFFIX`.
pub const CREDENTIALS_SERVICE_SUFFIX: &str = "-credentials";

/// `security -i` reads stdin with a 4096-byte `fgets()` buffer (BUFSIZ on
/// darwin). Anything longer is truncated mid-argument. 64 B headroom matches
/// claude-code's safety margin (see macOsKeychainStorage.ts:24).
pub const SECURITY_STDIN_LINE_LIMIT: usize = 4096 - 64;

/// 30 s TTL on cached keychain reads. Claude-code's
/// `KEYCHAIN_CACHE_TTL_MS = 30_000`. Bounds cross-process staleness without
/// triggering repeated 500 ms `security` spawns under load.
pub const KEYCHAIN_CACHE_TTL: Duration = Duration::from_secs(30);

/// Produce the full keychain service name.
///
/// Mirrors claude-code's `getMacOsKeychainStorageServiceName`:
/// `"Claude Code" + oauth_suffix + service_suffix + dir_hash`.
///
/// `base` is conventionally `"Claude Code"`; we accept it as a parameter so
/// downstream products can override the prefix without touching this helper.
///
/// `oauth_suffix` is claude-code's `OAUTH_FILE_SUFFIX` (empty in stable;
/// non-empty in some build variants).
///
/// `service_suffix` is [`CREDENTIALS_SERVICE_SUFFIX`] for OAuth entries
/// or empty for the legacy API-key entry.
///
/// `dir_hash` is the output of [`compute_dir_hash`] — either empty (default
/// `~/.claude`) or `format!("-{}", &sha256(config_dir).hex()[..8])`.
#[must_use]
pub fn full_service_name(
    base: &str,
    oauth_suffix: &str,
    service_suffix: &str,
    dir_hash: &str,
) -> String {
    format!("{base}{oauth_suffix}{service_suffix}{dir_hash}")
}

/// Return the keychain-name dir-hash component.
///
/// Returns the empty string when `config_dir == default_dir`. Otherwise
/// returns `"-" + sha256(config_dir.to_string_lossy()).hex()[..8]`.
///
/// `default_dir` is the engine's canonical "default" config directory;
/// passing the actual user-home-derived default avoids env lookups inside
/// the helper. Claude-code's source uses `process.env.LINGXI_CONFIG_DIR`
/// presence as the discriminator, which is equivalent (when the env var is
/// unset, `getClaudeConfigHomeDir()` returns the default).
#[must_use]
pub fn compute_dir_hash(config_dir: &Path, default_dir: &Path) -> String {
    if config_dir == default_dir {
        return String::new();
    }
    let bytes = config_dir.to_string_lossy();
    let mut hasher = Sha256::new();
    hasher.update(bytes.as_bytes());
    let digest = hasher.finalize();
    let hex_full = hex::encode(digest);
    format!("-{}", &hex_full[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_is_30_seconds() {
        assert_eq!(KEYCHAIN_CACHE_TTL.as_secs(), 30);
    }

    #[test]
    fn stdin_limit_matches_claude_code() {
        assert_eq!(SECURITY_STDIN_LINE_LIMIT, 4096 - 64);
    }

    #[test]
    fn credentials_suffix_matches_claude_code() {
        assert_eq!(CREDENTIALS_SERVICE_SUFFIX, "-credentials");
    }

    #[test]
    fn dir_hash_default_returns_empty() {
        let p = Path::new("/Users/x/.lingxi");
        assert_eq!(compute_dir_hash(p, p), "");
    }
}
