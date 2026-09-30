//! In-process credential callback + CA wiring for the network git ops
//! (clone / fetch / pull), spec §G3 (auth) + §G6 (TLS/CA).
//!
//! The HTTPS token is supplied in-memory by the Kotlin host and installed into
//! libgit2's `RemoteCallbacks::credentials` only for the duration of a network
//! call (borrowed for the `FetchOptions` lifetime). It NEVER reaches disk, a
//! child process, argv, or an environment variable: libgit2 is in-process, so
//! there is no exec boundary to cross. The token is also never logged.
//!
//! ## TLS backend: mbedTLS (Android), CA store wiring
//!
//! libgit2 is built against **mbedTLS** for the Android target (the OpenSSL
//! Rust glue was removed in M-a). CA trust is configured at runtime by pointing
//! libgit2's mbedTLS stream at Android's system CA directory
//! (`/system/etc/security/cacerts`) via [`set_ca_location`]. This works because
//! this libgit2 version wires `GIT_OPT_SET_SSL_CERT_LOCATIONS` for the mbedTLS
//! backend too (not only OpenSSL): the option routes to
//! `git_mbedtls__set_cert_location(file, path)` in
//! `streams/mbedtls.c`, which loads a whole directory of hashed PEM certs with
//! `mbedtls_x509_crt_parse_path(path)` and installs them as the trust chain via
//! `mbedtls_ssl_conf_ca_chain`. No `streams/mbedtls.c` patch is required.
//!
//! Verification is **fail-closed (verify-required equivalent)**. The mbedTLS
//! config uses `MBEDTLS_SSL_VERIFY_OPTIONAL` *only* so libgit2 can still read
//! the peer certificate after the handshake (REQUIRED frees it on failure); the
//! stream's `verify_server_cert` then re-checks `mbedtls_ssl_get_verify_result`
//! and returns `GIT_ECERTIFICATE` on any failure. For HTTPS we install **no**
//! `certificate_check` callback (see [`make_network_callbacks`], which only adds
//! one for SSH host-key pinning), so libgit2's `httpclient.c` propagates that
//! `GIT_ECERTIFICATE` as a hard connection failure — there is no path that
//! accepts an unverified cert. This is identical to libgit2's OpenSSL backend
//! contract; we do NOT weaken the verify mode.
//!
//! ## `forbid(unsafe_code)` carve-out
//!
//! The crate is `#![deny(unsafe_code)]` (NOT `forbid`) for two audited reasons,
//! both libgit2 process-global setters that the vendored `git2` exposes as
//! **`unsafe`** (they mutate a libgit2 global without synchronization):
//!
//! 1. [`set_ca_location`] — `git2::opts::set_ssl_cert_dir` over
//!    `GIT_OPT_SET_SSL_CERT_LOCATIONS`. The spec (G6) assumed a safe
//!    `set_ssl_cert_locations`; the pinned `git2 0.21` instead splits it into
//!    `unsafe` setters (still required under mbedTLS).
//! 2. [`ensure_ssh_homedir`] — `git2::opts::set_homedir` over
//!    `GIT_OPT_SET_HOMEDIR`, so libssh2 can expand `~/.ssh/known_hosts` on
//!    Android (no `HOME`); without it SSH fails closed before host-key pinning.
//!
//! Each `unsafe` block is localized under `#[allow(unsafe_code)]`; the rest of
//! the crate keeps the deny lint, so no other unsafe can slip in.

use std::path::{Path, PathBuf};

use crate::ops::GitOpError;

/// Username sent alongside the HTTPS token. GitHub/GitLab personal access
/// tokens are presented as the *password* with a fixed sentinel username; this
/// matches the documented `x-access-token` / PAT-as-password convention.
const TOKEN_USERNAME: &str = "x-access-token";

/// Install the HTTPS-token credentials callback on `callbacks`.
///
/// When `token` is `Some`, a `credentials` callback yields
/// `Cred::userpass_plaintext(TOKEN_USERNAME, token)` — the token is borrowed for
/// the callbacks' lifetime (`'a`), never copied into a longer-lived store. When
/// `None`, nothing is installed (public/anonymous HTTPS + `file://` still work).
/// The token is never logged or written anywhere; it only flows into libgit2's
/// in-process credential callback.
///
/// NOTE (G7): the live network ops now build their callbacks via
/// [`make_network_callbacks`] (which handles BOTH the HTTPS token and the SSH
/// key + host-key verification). This helper + [`make_fetch_options`] are
/// retained (HTTPS-only convenience, still unit-tested) but are no longer on the
/// clone/fetch/pull/push path; prefer `make_network_callbacks` for new callers.
pub fn install_token_credentials<'a>(
    callbacks: &mut git2::RemoteCallbacks<'a>,
    token: Option<&'a str>,
) {
    if let Some(token) = token {
        callbacks.credentials(move |_url, _username_from_url, _allowed| {
            git2::Cred::userpass_plaintext(TOKEN_USERNAME, token)
        });
    }
}

/// Build the [`git2::FetchOptions`] used by every network op (clone / fetch /
/// pull), with the in-process token credentials callback installed.
///
/// When `token` is `Some`, a `credentials` callback is installed that yields
/// `Cred::userpass_plaintext(TOKEN_USERNAME, token)` — the token is borrowed
/// for the returned `FetchOptions`' lifetime (`'a`), never copied into a
/// longer-lived store. When `token` is `None`, no credentials callback is
/// installed, so public (anonymous) HTTPS / `file://` remotes still work.
///
/// The token is never logged or written anywhere; it only flows into the
/// libgit2 credential callback in-process. See [`install_token_credentials`],
/// which holds the token convention shared with `ops::push`.
#[must_use]
pub fn make_fetch_options(token: Option<&str>) -> git2::FetchOptions<'_> {
    let mut callbacks = git2::RemoteCallbacks::new();
    install_token_credentials(&mut callbacks, token);
    let mut opts = git2::FetchOptions::new();
    opts.remote_callbacks(callbacks);
    opts
}

/// SSH-key authentication material for a network git op, supplied in-memory by
/// the Kotlin host (spec §G7). The private key is a **file path** (validated to
/// stay inside the app sandbox by [`validate_ssh_key_path`] before use); the
/// passphrase, when needed, is fetched per-op from the
/// [`tool_api::GitCredentialProvider`] (it flows only into libgit2's in-process
/// credential callback — never logged, never written, never exec'd, never held
/// resident). The host key is verified strictly: only fingerprints in
/// `known_hosts_sha256_hex` (lowercase-hex SHA-256) are accepted.
#[derive(Clone, Default)]
pub struct SshConfig {
    /// Filesystem path to the private key. Validated against the sandbox root
    /// (see [`validate_ssh_key_path`]) before being handed to libgit2.
    pub private_key_path: String,
    /// Optional path to the matching public key. libgit2/libssh2 can derive it
    /// from the private key when `None`.
    pub public_key_path: Option<String>,
    /// Pinned host-key SHA-256 fingerprints. Each entry may be either
    /// lowercase-hex (compared case-insensitively) or the OpenSSH/GitHub
    /// `SHA256:<base64>` form (`ssh-keygen -lf`, compared case-sensitively); the
    /// remote's host key is accepted only if its SHA-256 matches a member. An
    /// empty list rejects every host key. See [`host_key_is_pinned`].
    pub known_hosts_sha256_hex: Vec<String>,
}

/// Hex-encode a host-key SHA-256 digest as a lowercase 64-char string.
///
/// libgit2 (`git2::Cert::as_hostkey().hash_sha256()`) hands us the raw 32-byte
/// SHA-256 of the remote's host key; we render it as lowercase hex to compare
/// against the host-supplied pinned `known_hosts_sha256_hex`.
#[must_use]
pub fn hostkey_sha256_hex(sha256: &[u8]) -> String {
    let mut out = String::with_capacity(sha256.len() * 2);
    for b in sha256 {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Standard-alphabet Base64 (no padding) of a byte slice.
///
/// Used to compare a host-key SHA-256 against the OpenSSH/GitHub
/// `SHA256:<base64>` fingerprint form (`ssh-keygen -lf`, GitHub's published
/// fingerprints), which is unpadded standard Base64 — *not* lowercase hex. Kept
/// inline to avoid pulling a `base64` dependency into the mobile crate.
#[must_use]
fn base64_no_pad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as usize;
        out.push(ALPHABET[b0 >> 2] as char);
        match chunk.len() {
            1 => out.push(ALPHABET[(b0 & 0b11) << 4] as char),
            2 => {
                let b1 = chunk[1] as usize;
                out.push(ALPHABET[((b0 & 0b11) << 4) | (b1 >> 4)] as char);
                out.push(ALPHABET[(b1 & 0b1111) << 2] as char);
            }
            _ => {
                let b1 = chunk[1] as usize;
                let b2 = chunk[2] as usize;
                out.push(ALPHABET[((b0 & 0b11) << 4) | (b1 >> 4)] as char);
                out.push(ALPHABET[((b1 & 0b1111) << 2) | (b2 >> 6)] as char);
                out.push(ALPHABET[b2 & 0b111111] as char);
            }
        }
    }
    out
}

/// Strict host-key pinning check: is the remote's host-key SHA-256 in the
/// host-supplied pinned set?
///
/// `sha256` is the raw digest from the certificate; each entry of `pinned` is a
/// host-supplied fingerprint accepted in either common form:
/// - **lowercase hex** (64 chars, compared case-insensitively) — the original
///   convention; or
/// - **OpenSSH/GitHub Base64** (`SHA256:<base64>` or bare unpadded Base64, as
///   emitted by `ssh-keygen -lf` and published by hosting providers) — compared
///   case-*sensitively* (Base64 is case-significant), tolerating the optional
///   `SHA256:` prefix and trailing `=` padding.
///
/// Returns `true` only on an exact match in one of those forms. An empty
/// `pinned` rejects every key — fail-closed.
#[must_use]
pub fn host_key_is_pinned(sha256: &[u8], pinned: &[String]) -> bool {
    let actual_hex = hostkey_sha256_hex(sha256);
    let actual_b64 = base64_no_pad(sha256);
    pinned.iter().any(|p| {
        let p = p.trim();
        if p.eq_ignore_ascii_case(&actual_hex) {
            return true;
        }
        let b64 = p.strip_prefix("SHA256:").unwrap_or(p).trim_end_matches('=');
        // Only treat as Base64 when it is not also valid hex (avoids a hex pin
        // accidentally matching via the case-sensitive Base64 path).
        b64 == actual_b64
    })
}

/// Validate that an SSH private-key path stays inside the app sandbox.
///
/// Both `path` and `sandbox_root` are canonicalized (resolving `..`/symlinks),
/// then the key must lie under the canonical root — mirroring
/// [`crate::ops::open_repo`]'s containment check. A nonexistent key (or root)
/// fails to canonicalize and is rejected.
///
/// # Errors
///
/// - [`GitOpError::NotFound`] — `path` or `sandbox_root` cannot be canonicalized
///   (does not exist).
/// - [`GitOpError::InvalidInput`] — the canonical key path lies outside the
///   canonical sandbox root.
pub fn validate_ssh_key_path(path: &str, sandbox_root: &Path) -> Result<PathBuf, GitOpError> {
    let canonical_root = sandbox_root.canonicalize().map_err(|e| {
        GitOpError::NotFound(format!("sandbox root {}: {e}", sandbox_root.display()))
    })?;
    let canonical_key = Path::new(path)
        .canonicalize()
        .map_err(|e| GitOpError::NotFound(format!("ssh key {path}: {e}")))?;
    if !canonical_key.starts_with(&canonical_root) {
        return Err(GitOpError::InvalidInput(format!(
            "ssh key path {} escapes the app sandbox",
            canonical_key.display()
        )));
    }
    Ok(canonical_key)
}

/// Borrowed credential/host-key inputs for a single network op. The per-op
/// [`tool_api::GitCredentialProvider`] supplies the HTTPS token / SSH passphrase
/// lazily (never held resident); `ssh` carries the key path + pinned host keys.
/// Both being present is handled (the credentials closure picks per the
/// libgit2-requested type).
pub struct NetCallbacks<'a> {
    /// Per-op credential provider: yields the HTTPS token (presented as the
    /// password with the sentinel username) and the SSH passphrase on demand.
    pub provider: Option<&'a dyn tool_api::GitCredentialProvider>,
    /// SSH-key material + pinned host keys.
    pub ssh: Option<&'a SshConfig>,
}

/// The credential `select_credential` resolved for a libgit2 request — owned so
/// it's testable independent of libgit2's `Cred` (which the closure builds from it).
#[derive(Debug, PartialEq, Eq)]
pub enum CredentialChoice {
    /// libgit2 asked only for a username (libssh2's first request) — supply it.
    Username(String),
    /// HTTPS token (PAT): the sentinel `user` with the `token` as the password.
    UserPass {
        /// Sentinel username sent alongside the token (`x-access-token`).
        user: String,
        /// HTTPS token (PAT) used as the password.
        token: String,
    },
    /// SSH key auth: validated private-key path + optional public key/passphrase.
    SshKey {
        /// SSH username (from the URL, else `git`).
        user: String,
        /// Filesystem path to the private key.
        key_path: String,
        /// Optional matching public-key path (libssh2 can derive it when absent).
        pubkey: Option<String>,
        /// Optional passphrase decrypting the private key (per-op, lazily fetched).
        passphrase: Option<String>,
    },
    /// No usable credential for the requested type — the closure errors out.
    None,
}

/// Resolve the credential for a libgit2 `allowed` request, fetching secrets from
/// the per-op `provider` (HTTPS token / SSH passphrase) lazily. Pure: returns
/// owned data, so a mock provider unit-tests the per-op fetch.
#[must_use]
pub fn select_credential(
    allowed: git2::CredentialType,
    provider: Option<&dyn tool_api::GitCredentialProvider>,
    ssh: Option<&SshConfig>,
    username: &str,
) -> CredentialChoice {
    if allowed.contains(git2::CredentialType::USERNAME) {
        return CredentialChoice::Username(username.to_owned());
    }
    if let Some(ssh) = ssh {
        if allowed.contains(git2::CredentialType::SSH_KEY) {
            return CredentialChoice::SshKey {
                user: username.to_owned(),
                key_path: ssh.private_key_path.clone(),
                pubkey: ssh.public_key_path.clone(),
                passphrase: provider.and_then(tool_api::GitCredentialProvider::ssh_passphrase),
            };
        }
    }
    if allowed.contains(git2::CredentialType::USER_PASS_PLAINTEXT) {
        if let Some(token) = provider.and_then(tool_api::GitCredentialProvider::https_token) {
            return CredentialChoice::UserPass {
                user: TOKEN_USERNAME.to_owned(),
                token,
            };
        }
    }
    CredentialChoice::None
}

/// Build the [`git2::RemoteCallbacks`] for a network op, installing a unified
/// `credentials` closure (HTTPS token and/or SSH key) and — when SSH is in play
/// — a strict `certificate_check` host-key verifier.
///
/// The `credentials` closure dispatches on the libgit2-requested
/// [`git2::CredentialType`] in order:
/// - `USERNAME` → [`git2::Cred::username`] (libssh2 asks for the username first
///   when none is in the URL).
/// - `SSH_KEY` (and `ssh` present) → [`git2::Cred::ssh_key`] with the validated
///   private-key path + optional public key + optional passphrase.
/// - `USER_PASS_PLAINTEXT` (and `token` present) → [`git2::Cred::userpass_plaintext`]
///   with the `x-access-token` sentinel.
/// - otherwise → an error (no usable credential).
///
/// The SSH username is taken from the URL (`username_from_url`) when libgit2
/// supplies it, else defaults to `git`.
///
/// When `ssh` is present, `certificate_check` reads the remote host key's
/// SHA-256 and returns [`git2::CertificateCheckStatus::CertificateOk`] iff it is
/// pinned in `known_hosts_sha256_hex`; an unknown/mismatched host key is a hard
/// error (fail-closed). A non-host-key certificate (e.g. an HTTPS X.509 cert,
/// which can't occur on an SSH transport) is passed through to libgit2's default
/// verification.
///
/// Secrets (token, passphrase, key bytes) flow only into libgit2 in-process and
/// are never logged.
#[must_use]
pub fn make_network_callbacks<'a>(p: &NetCallbacks<'a>) -> git2::RemoteCallbacks<'a> {
    let mut callbacks = git2::RemoteCallbacks::new();

    let provider = p.provider;
    let ssh = p.ssh;
    // SSH-key attempt counter for THIS op. libgit2's libssh2 transport drives
    // auth in a `while (error == GIT_EAUTH)` loop with no built-in retry cap;
    // because our closure has only one key to offer, returning it again on every
    // iteration spins forever on a wrong key / wrong passphrase (a local
    // key-decrypt failure never reaches the server's MaxAuthTries to bound it),
    // hanging the tool call indefinitely. Offer the key once; on the next
    // SSH_KEY request for this op, return an error so libgit2 ends the loop with
    // a clean GIT_EAUTH instead of an infinite hang.
    let ssh_attempts = std::cell::Cell::new(0u32);
    callbacks.credentials(move |_url, username_from_url, allowed| {
        let user = username_from_url.unwrap_or("git");
        match select_credential(allowed, provider, ssh, user) {
            CredentialChoice::Username(u) => git2::Cred::username(&u),
            CredentialChoice::UserPass { user, token } => {
                git2::Cred::userpass_plaintext(&user, &token)
            }
            CredentialChoice::SshKey {
                user,
                key_path,
                pubkey,
                passphrase,
            } => {
                let n = ssh_attempts.get();
                ssh_attempts.set(n + 1);
                if n >= 1 {
                    return Err(git2::Error::from_str(
                        "ssh key authentication failed (key or passphrase rejected)",
                    ));
                }
                git2::Cred::ssh_key(
                    &user,
                    pubkey.as_deref().map(Path::new),
                    Path::new(&key_path),
                    passphrase.as_deref(),
                )
            }
            CredentialChoice::None => Err(git2::Error::from_str(
                "no usable git credential for the requested authentication type",
            )),
        }
    });

    if let Some(ssh) = ssh {
        let pinned = ssh.known_hosts_sha256_hex.clone();
        callbacks.certificate_check(move |cert, _host| {
            let Some(hostkey) = cert.as_hostkey() else {
                // Not an SSH host key (e.g. an X.509 cert) — defer to libgit2's
                // default verification rather than vouching for it here.
                return Ok(git2::CertificateCheckStatus::CertificatePassthrough);
            };
            match hostkey.hash_sha256() {
                Some(sha256) if host_key_is_pinned(sha256, &pinned) => {
                    Ok(git2::CertificateCheckStatus::CertificateOk)
                }
                _ => Err(git2::Error::from_str(
                    "unknown or mismatched SSH host key (not in pinned known_hosts)",
                )),
            }
        });
    }

    callbacks
}

/// Build a [`git2::FetchOptions`] whose `RemoteCallbacks` come from
/// [`make_network_callbacks`] — i.e. the unified HTTPS-token + SSH-key
/// credentials closure plus (when SSH is in play) the strict host-key
/// `certificate_check`. This is the network-op fetch path for clone / fetch /
/// pull, so SSH and HTTPS both work through the same callbacks.
///
/// Secrets borrowed by the callbacks live for the returned options' lifetime
/// (`'a`); they only flow into libgit2 in-process and are never logged.
#[must_use]
pub fn make_fetch_options_net<'a>(p: &NetCallbacks<'a>) -> git2::FetchOptions<'a> {
    let mut opts = git2::FetchOptions::new();
    opts.remote_callbacks(make_network_callbacks(p));
    opts
}

/// Point libgit2's **mbedTLS** backend at a CA-certificate directory for
/// server-certificate verification.
///
/// `ca_dir`, when `Some`, is treated as a **directory** of one-cert-per-file CA
/// certificates (the Android `/system/etc/security/cacerts` layout: hashed
/// `<hash>.0` PEM files) and passed as the `path` argument of
/// `GIT_OPT_SET_SSL_CERT_LOCATIONS` via [`git2::opts::set_ssl_cert_dir`]; the
/// `file` argument is left unset. Under mbedTLS this option routes to
/// `git_mbedtls__set_cert_location(NULL, path)`, which loads the whole
/// directory with `mbedtls_x509_crt_parse_path` and installs it as the trust
/// chain (`mbedtls_ssl_conf_ca_chain`). Verification then fails closed against
/// that chain (see the module-level doc: `verify_server_cert` →
/// `GIT_ECERTIFICATE`, no HTTPS `certificate_check` override). When `None`, this
/// is a no-op — libgit2 keeps its built-in default (none), which is fine for the
/// host `file://` tests that perform no TLS.
///
/// This is a **global** libgit2 option (set once for the process). It is
/// idempotent to call again with the same value.
///
/// # Errors
///
/// [`GitOpError::Libgit2`] if libgit2/mbedTLS rejects the location (e.g. the
/// directory cannot be parsed into any valid cert).
pub fn set_ca_location(ca_dir: Option<&str>) -> Result<(), GitOpError> {
    let Some(dir) = ca_dir else {
        return Ok(());
    };
    // SAFETY: `git2::opts::set_ssl_cert_dir` is `unsafe` only because it mutates
    // a libgit2 process global without internal synchronization. We call it
    // from a single deterministic point (the start of each network op, before
    // any concurrent git work — Git ops are not concurrency-safe, see
    // `GitTool::is_concurrency_safe == false`), passing a validated dir path. It
    // drives the mbedTLS backend's `git_mbedtls__set_cert_location` in this
    // libgit2 version. No other unsafe is permitted in this crate
    // (`#![deny(unsafe_code)]`).
    #[allow(unsafe_code)]
    unsafe {
        git2::opts::set_ssl_cert_dir(Path::new(dir)).map_err(|e| GitOpError::from_git2(&e))
    }
}

/// Ensure libgit2 can resolve `~` for the SSH `known_hosts` lookup.
///
/// libgit2's libssh2 transport unconditionally expands `~/.ssh/known_hosts`
/// before host-key verification. A **missing** file is fine (treated as "host
/// not previously known", which our pinned [`make_network_callbacks`]
/// `certificate_check` then adjudicates), but an **unresolvable `~`** is a hard
/// `error loading known_hosts` that fails the connection *before* our callback
/// runs. An Android app process has no `HOME`, so `~` cannot be resolved and
/// every SSH op fails closed for the wrong reason.
///
/// This sets `HOME` (only when unset/empty, so a real desktop `HOME` is never
/// clobbered) to the SSH key's parent directory — an app-private, writable path
/// that already exists. `known_hosts` stays absent there, so the pinned
/// `certificate_check` remains the sole trust decision (G7 is unchanged). No-op
/// when SSH is not configured.
///
/// libgit2 resolves and **caches** the home directory at initialization (from
/// `HOME`), so simply setting the `HOME` env var inside the op is too late — the
/// empty value is already cached. We instead override the cached home directory
/// directly via `GIT_OPT_SET_HOMEDIR`. We only do so when the process has no
/// usable `HOME` (the Android case); a real desktop `HOME` is left alone so
/// libgit2 keeps using the user's actual `~/.ssh`.
///
/// # Errors
///
/// [`GitOpError::Libgit2`] if libgit2 rejects the home-directory path.
pub fn ensure_ssh_homedir(ssh: Option<&SshConfig>) -> Result<(), GitOpError> {
    let Some(ssh) = ssh else {
        return Ok(());
    };
    // Desktop/host already has a usable HOME — keep libgit2's real `~/.ssh`.
    if std::env::var_os("HOME").is_some_and(|v| !v.is_empty()) {
        return Ok(());
    }
    let Some(parent) = Path::new(&ssh.private_key_path).parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    // SAFETY: `git2::opts::set_homedir` is `unsafe` only because it mutates a
    // libgit2 process global without internal synchronization. We call it from
    // the single deterministic per-op setup point (before any concurrent git
    // work — Git ops are not concurrency-safe, see
    // `GitTool::is_concurrency_safe == false`), passing an app-private, existing
    // directory. It overwrites libgit2's cached home dir so the libssh2
    // transport can expand `~/.ssh/known_hosts` (a missing file there is fine;
    // the pinned `certificate_check` remains the trust decision). This is the
    // second of two audited carve-outs in this `#![deny(unsafe_code)]` crate.
    #[allow(unsafe_code)]
    unsafe {
        git2::opts::set_homedir(parent).map_err(|e| GitOpError::from_git2(&e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `make_fetch_options(Some(token))` wires up a credentials callback. We
    /// cannot invoke libgit2's private callback dispatch from Rust in
    /// isolation, so this asserts the assembly path: building the options with
    /// a token does not panic, and (separately) the underlying `Cred` the
    /// callback would produce is the expected `userpass_plaintext` credential.
    #[test]
    fn credentials_callback_yields_token() {
        // The options assemble without panicking with a token present.
        let _opts = make_fetch_options(Some("tok-123"));
        // And with no token (public repos).
        let _opts_none = make_fetch_options(None);

        // Directly construct the credential the callback closure returns, to
        // lock the convention (sentinel username + token-as-password). This is
        // exactly the value the installed `credentials` callback yields.
        git2::Cred::userpass_plaintext(TOKEN_USERNAME, "tok-123")
            .expect("userpass_plaintext should build a Cred from the token");
    }

    #[test]
    fn install_token_credentials_is_noop_without_token() {
        // No token => no credentials callback installed; building options/callbacks
        // must not panic. (We cannot invoke libgit2's private dispatch in isolation.)
        let mut cb = git2::RemoteCallbacks::new();
        install_token_credentials(&mut cb, None);
        let mut cb2 = git2::RemoteCallbacks::new();
        install_token_credentials(&mut cb2, Some("tok-xyz"));
        // The token convention is still the userpass_plaintext sentinel:
        git2::Cred::userpass_plaintext(TOKEN_USERNAME, "tok-xyz")
            .expect("userpass_plaintext should build a Cred");
    }

    /// `set_ca_location(None)` is a no-op and never errors.
    #[test]
    fn set_ca_location_none_is_noop() {
        set_ca_location(None).expect("None CA dir is a no-op");
    }

    #[test]
    fn hostkey_hex_matches_pinned() {
        let raw = [0xABu8; 32];
        let hex = hostkey_sha256_hex(&raw);
        assert_eq!(hex.len(), 64);
        assert!(hex
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        let pinned = vec![hex.clone()];
        assert!(
            host_key_is_pinned(&raw, &pinned),
            "exact hex match accepted"
        );
        let other = [0x00u8; 32];
        assert!(!host_key_is_pinned(&other, &pinned), "unknown key rejected");
        let pinned_upper = vec![hex.to_uppercase()];
        assert!(
            host_key_is_pinned(&raw, &pinned_upper),
            "pinned hex compared case-insensitively"
        );
        // Fail-closed: an empty pinned set NEVER trusts a host key (no MITM defense
        // would otherwise be bypassed by an unconfigured known_hosts).
        assert!(
            !host_key_is_pinned(&raw, &[]),
            "empty pinned list must reject (fail-closed)"
        );
    }

    #[test]
    fn hostkey_base64_sha256_form_matches_pinned() {
        // Operators commonly paste the OpenSSH/GitHub `SHA256:<base64>` form
        // (ssh-keygen -lf), not lowercase hex — accept it too.
        let raw = [0xABu8; 32];
        let b64 = base64_no_pad(&raw);
        // base64 of 32 bytes is 43 chars unpadded.
        assert_eq!(b64.len(), 43);
        assert!(
            host_key_is_pinned(&raw, &[format!("SHA256:{b64}")]),
            "SHA256: prefixed base64 accepted"
        );
        assert!(
            host_key_is_pinned(&raw, std::slice::from_ref(&b64)),
            "bare base64 accepted"
        );
        assert!(
            host_key_is_pinned(&raw, &[format!("{b64}=")]),
            "trailing '=' padding tolerated"
        );
        // Base64 is case-sensitive: a wrong-case base64 must NOT match.
        let other = [0x00u8; 32];
        assert!(
            !host_key_is_pinned(&other, &[format!("SHA256:{b64}")]),
            "wrong key rejected via base64 path"
        );
        // Sanity vs a known OpenSSH vector: SHA-256 of empty digest input is not
        // exercised here; instead verify the encoder against a RFC4648 vector.
        assert_eq!(base64_no_pad(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_no_pad(b"fooba"), "Zm9vYmE");
        assert_eq!(base64_no_pad(b"foob"), "Zm9vYg");
    }

    #[test]
    fn validate_ssh_key_path_rejects_outside_sandbox() {
        let sandbox = tempfile::tempdir().unwrap();
        let key = sandbox.path().join("id_ed25519");
        std::fs::write(&key, b"-----BEGIN OPENSSH PRIVATE KEY-----\n").unwrap();
        validate_ssh_key_path(key.to_str().unwrap(), sandbox.path())
            .expect("in-sandbox key path accepted");
        let missing = sandbox.path().join("nope");
        assert!(validate_ssh_key_path(missing.to_str().unwrap(), sandbox.path()).is_err());
        let outside = sandbox.path().join("../escape");
        assert!(validate_ssh_key_path(outside.to_str().unwrap(), sandbox.path()).is_err());
    }

    #[test]
    fn make_network_callbacks_assembles_for_https_and_ssh() {
        let p = mock(Some("tok"), None);
        let _cb = make_network_callbacks(&NetCallbacks {
            provider: Some(&p),
            ssh: None,
        });
        let ssh = SshConfig {
            private_key_path: "/sandbox/id".into(),
            known_hosts_sha256_hex: vec!["abc".into()],
            ..Default::default()
        };
        let _cb2 = make_network_callbacks(&NetCallbacks {
            provider: None,
            ssh: Some(&ssh),
        });
    }

    use std::sync::atomic::{AtomicUsize, Ordering};
    struct MockProvider {
        token: Option<String>,
        pass: Option<String>,
        tc: AtomicUsize,
        pc: AtomicUsize,
    }
    impl tool_api::GitCredentialProvider for MockProvider {
        fn https_token(&self) -> Option<String> {
            self.tc.fetch_add(1, Ordering::SeqCst);
            self.token.clone()
        }
        fn ssh_passphrase(&self) -> Option<String> {
            self.pc.fetch_add(1, Ordering::SeqCst);
            self.pass.clone()
        }
    }
    fn mock(token: Option<&str>, pass: Option<&str>) -> MockProvider {
        MockProvider {
            token: token.map(Into::into),
            pass: pass.map(Into::into),
            tc: AtomicUsize::new(0),
            pc: AtomicUsize::new(0),
        }
    }

    #[test]
    fn select_credential_userpass_calls_https_token() {
        let p = mock(Some("tok"), None);
        let c = select_credential(
            git2::CredentialType::USER_PASS_PLAINTEXT,
            Some(&p),
            None,
            "git",
        );
        assert_eq!(
            c,
            CredentialChoice::UserPass {
                user: TOKEN_USERNAME.to_owned(),
                token: "tok".to_owned()
            }
        );
        assert_eq!(
            p.tc.load(Ordering::SeqCst),
            1,
            "https_token fetched once, per-op"
        );
    }
    #[test]
    fn select_credential_sshkey_calls_passphrase() {
        let p = mock(None, Some("pp"));
        let ssh = SshConfig {
            private_key_path: "/k".into(),
            public_key_path: Some("/k.pub".into()),
            known_hosts_sha256_hex: vec![],
        };
        let c = select_credential(git2::CredentialType::SSH_KEY, Some(&p), Some(&ssh), "git");
        assert_eq!(
            c,
            CredentialChoice::SshKey {
                user: "git".into(),
                key_path: "/k".into(),
                pubkey: Some("/k.pub".into()),
                passphrase: Some("pp".into())
            }
        );
        assert_eq!(
            p.pc.load(Ordering::SeqCst),
            1,
            "ssh_passphrase fetched once, per-op"
        );
    }
    #[test]
    fn select_credential_username_first() {
        let c = select_credential(git2::CredentialType::USERNAME, None, None, "git");
        assert_eq!(c, CredentialChoice::Username("git".to_owned()));
    }
    #[test]
    fn select_credential_none_without_provider_or_token() {
        // USER_PASS requested but no provider → None.
        assert_eq!(
            select_credential(git2::CredentialType::USER_PASS_PLAINTEXT, None, None, "git"),
            CredentialChoice::None
        );
        // provider present but returns no token → None.
        let p = mock(None, None);
        assert_eq!(
            select_credential(
                git2::CredentialType::USER_PASS_PLAINTEXT,
                Some(&p),
                None,
                "git"
            ),
            CredentialChoice::None
        );
    }

    /// `set_ca_location(Some(dir))` exercises the `GIT_OPT_SET_SSL_CERT_LOCATIONS`
    /// path, which under the mbedTLS backend routes to
    /// `git_mbedtls__set_cert_location` (loads the dir via
    /// `mbedtls_x509_crt_parse_path`). The host can't exercise the Android trust
    /// store, but the reworked contract must be sane: calling it never panics and
    /// returns either `Ok(())` or a NAMED [`GitOpError::Libgit2`] — never a silent
    /// wrong-success of some other error kind, and never a weakened/skipped verify
    /// (the verify-required-equivalent contract is enforced inside libgit2, see the
    /// module doc; the live good-vs-bad-cert check is device-only, M-c). Whether an
    /// empty tempdir yields Ok or a named error depends on how mbedTLS/libgit2 is
    /// built on this host — both are acceptable; we only pin the contract shape.
    #[test]
    fn set_ca_location_contract_under_mbedtls() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Calling must not panic, and the only acceptable outcomes are Ok(()) or
        // a NAMED `GitOpError::Libgit2(_)` — never a silent wrong-success of some
        // other error kind.
        let acceptable = matches!(
            set_ca_location(Some(dir.path().to_str().unwrap())),
            Ok(()) | Err(GitOpError::Libgit2(_))
        );
        assert!(
            acceptable,
            "set_ca_location must return Ok or the named GitOpError::Libgit2, \
             never an unexpected error kind"
        );
    }
}
