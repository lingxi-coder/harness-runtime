//! mTLS client-identity + custom CA-trust configuration for the shared reqwest
//! transport.
//!
//! Parity: claude-code 2.1.207
//! * `k5()` — the memoized mTLS config builder that reads
//!   `CLAUDE_CODE_CLIENT_CERT` / `CLAUDE_CODE_CLIENT_KEY` (PEM file paths) plus
//!   `CLAUDE_CODE_CLIENT_KEY_PASSPHRASE`, and hands `{cert,key,passphrase}` to
//!   Node's `https.Agent`.
//! * `JEm()` — the `CLAUDE_CODE_CERT_STORE` parser that produces the ordered,
//!   de-duplicated `["bundled"|"system"]` trust-store source list (default
//!   `["bundled","system"]`).
//! * `zz` — the CA-roots loader that assembles bundled (webpki) + system roots
//!   and appends `NODE_EXTRA_CA_CERTS`.
//!
//! ## Rebrand
//!
//! Per the repo convention (`CLAUDE_CODE_X` → `LINGXI_X`, cf.
//! `LINGXI_DISABLE_FAST_MODE`, `LINGXI_MAX_OUTPUT_TOKENS`) the rebranded
//! `LINGXI_*` spelling is read first and the upstream `CLAUDE_CODE_*` spelling
//! is honored as a fallback so existing corporate configs keep working.
//! `NODE_EXTRA_CA_CERTS` is a Node/OpenSSL standard variable and is kept
//! verbatim (matching `sandbox-runtime`'s `CA_TRUST_VARS` and `llm-runtime`'s
//! SSL hint copy).
//!
//! ## Node → rustls translation
//!
//! Node's `https.Agent` takes the cert/key PEMs (and decrypts an encrypted key
//! natively via `passphrase`). reqwest's rustls backend needs an already-built
//! [`reqwest::Identity`] whose private key is **unencrypted**, so an encrypted
//! (`ENCRYPTED PRIVATE KEY`) PKCS#8 key is decrypted here via the `pkcs8` crate
//! before the cert+key are concatenated into a single PEM identity blob.
//!
//! ## Deferred (see the H-BIN-07 dossier for the remainder)
//!
//! * `ca_certs_load` / mTLS telemetry counters (`Ze`/`we`/`Ee`) — observability
//!   only; would pull the telemetry crate into this foundational transport.
//! * Expired-system-cert dropping (`CA certs: Dropped N expired…`) — rustls
//!   rejects expired roots at verification time regardless.
//! * `--use-system-ca` / `--use-openssl-ca` CLI-flag → `["system"]` mapping —
//!   this transport crate has no argv; belongs to the CLI wiring layer.
//! * settings-env-change agent rebuild (`XY()`), `/doctor` diagnostic rows, and
//!   startup-telemetry fields — cross-cutting surfaces outside the transport.

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A CA trust-store source — one element of `JEm()`'s output list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CertSource {
    /// The webpki bundled root set (reqwest's built-in roots).
    Bundled,
    /// The host OS trust store (`rustls-native-certs`).
    System,
}

/// `JEm()`'s default source list `qsl = ["bundled","system"]`.
fn default_sources() -> Vec<CertSource> {
    vec![CertSource::Bundled, CertSource::System]
}

/// Parse the `LINGXI_CERT_STORE` / `CLAUDE_CODE_CERT_STORE` value (`JEm()`).
///
/// Comma-separated, trimmed + lowercased, `bundled`/`system` de-duplicated in
/// first-seen order. An unrecognized non-empty token logs the byte-faithful
/// warn line and is ignored. When nothing valid parses (or the value is
/// unset/empty) the default `["bundled","system"]` list is returned.
pub(crate) fn parse_cert_store(raw: Option<&str>) -> Vec<CertSource> {
    // `if(e)` in `JEm()` is falsy for an empty string, so treat "" as unset.
    let Some(value) = raw.filter(|s| !s.is_empty()) else {
        return default_sources();
    };
    let mut out: Vec<CertSource> = Vec::new();
    for part in value.split(',') {
        let token = part.trim().to_ascii_lowercase();
        match token.as_str() {
            "bundled" => {
                if !out.contains(&CertSource::Bundled) {
                    out.push(CertSource::Bundled);
                }
            }
            "system" => {
                if !out.contains(&CertSource::System) {
                    out.push(CertSource::System);
                }
            }
            "" => {}
            other => {
                tracing::warn!(
                    "CA certs: unrecognized LINGXI_CERT_STORE source '{other}', ignoring"
                );
            }
        }
    }
    if out.is_empty() {
        default_sources()
    } else {
        out
    }
}

/// The resolved mTLS / CA-trust environment (`k5()` + `JEm()` inputs +
/// `NODE_EXTRA_CA_CERTS`). Constructed once from the process environment when
/// the transport is built; kept as data so the application logic
/// ([`Self::apply_to_builder`]) is unit-testable without touching real env.
#[derive(Debug, Clone, Default)]
pub(crate) struct TlsSettings {
    /// `CLIENT_CERT`: PEM file path of the client certificate chain.
    pub client_cert: Option<PathBuf>,
    /// `CLIENT_KEY`: PEM file path of the client private key.
    pub client_key: Option<PathBuf>,
    /// `CLIENT_KEY_PASSPHRASE`: passphrase for an encrypted PKCS#8 client key.
    pub client_key_passphrase: Option<String>,
    /// `CERT_STORE`: raw trust-store source list value.
    pub cert_store: Option<String>,
    /// `NODE_EXTRA_CA_CERTS`: PEM file path of extra CA certificate(s).
    pub extra_ca_certs: Option<PathBuf>,
}

/// Read `lingxi` first, then the upstream `claude` spelling; empty values count
/// as unset (matching Node's `if(env)` truthiness).
fn dual_env(lingxi: &str, claude: &str) -> Option<String> {
    std::env::var(lingxi)
        .or_else(|_| std::env::var(claude))
        .ok()
        .filter(|s| !s.is_empty())
}

impl TlsSettings {
    /// Snapshot the mTLS / CA-trust environment.
    pub fn from_env() -> Self {
        Self {
            client_cert: dual_env("LINGXI_CLIENT_CERT", "CLAUDE_CODE_CLIENT_CERT")
                .map(PathBuf::from),
            client_key: dual_env("LINGXI_CLIENT_KEY", "CLAUDE_CODE_CLIENT_KEY").map(PathBuf::from),
            client_key_passphrase: dual_env(
                "LINGXI_CLIENT_KEY_PASSPHRASE",
                "CLAUDE_CODE_CLIENT_KEY_PASSPHRASE",
            ),
            cert_store: dual_env("LINGXI_CERT_STORE", "CLAUDE_CODE_CERT_STORE"),
            extra_ca_certs: std::env::var("NODE_EXTRA_CA_CERTS")
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
        }
    }

    /// Whether any mTLS identity material is configured — used by callers that
    /// skip a warm-up preflight when a client certificate is in play (`k5()`
    /// truthiness gate).
    #[cfg(test)]
    pub fn has_client_identity(&self) -> bool {
        self.client_cert.is_some() && self.client_key.is_some()
    }

    /// Apply the mTLS identity + custom CA roots to a reqwest client builder.
    ///
    /// Mirrors `k5()` (identity) followed by the `zz` CA-roots assembly:
    /// `bundled` keeps reqwest's built-in webpki roots, `system` adds the OS
    /// trust store, and `NODE_EXTRA_CA_CERTS` is appended when present.
    pub fn apply_to_builder(&self, mut builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
        if let Some(identity) = self.client_identity() {
            builder = builder.identity(identity);
        }

        let sources = parse_cert_store(self.cert_store.as_deref());
        let bundled = sources.contains(&CertSource::Bundled);
        let system = sources.contains(&CertSource::System);

        // Drop the webpki bundled roots only when "bundled" is not selected.
        builder = builder.tls_built_in_root_certs(bundled);
        if system {
            builder = add_system_roots(builder);
        }
        if let Some(path) = &self.extra_ca_certs {
            builder = add_extra_ca_certs(builder, path);
        }
        builder
    }

    /// Build the rustls configuration used by `tokio-tungstenite`.
    ///
    /// This deliberately assembles the same bundled/system/extra roots and
    /// client identity as [`Self::apply_to_builder`]. A configured but invalid
    /// client identity fails the WebSocket open instead of silently connecting
    /// without mTLS.
    pub fn websocket_client_config(&self) -> Result<Arc<rustls::ClientConfig>, String> {
        use rustls::pki_types::{
            CertificateDer, PrivateKeyDer, PrivatePkcs1KeyDer, PrivatePkcs8KeyDer,
            PrivateSec1KeyDer,
        };

        let sources = parse_cert_store(self.cert_store.as_deref());
        let mut roots = rustls::RootCertStore::empty();
        if sources.contains(&CertSource::Bundled) {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        if sources.contains(&CertSource::System) {
            let loaded = rustls_native_certs::load_native_certs();
            if !loaded.errors.is_empty() {
                tracing::warn!(
                    "CA certs: {} system root(s) failed to load: {:?}",
                    loaded.errors.len(),
                    loaded.errors
                );
            }
            let (added, ignored) = roots.add_parsable_certificates(loaded.certs);
            tracing::debug!(
                "CA certs: Loaded {added} system CA certificates for WebSocket ({ignored} ignored)"
            );
        }
        if let Some(path) = &self.extra_ca_certs {
            let bytes = std::fs::read(path).map_err(|error| {
                format!(
                    "failed to read NODE_EXTRA_CA_CERTS file ({}): {error}",
                    path.display()
                )
            })?;
            let blocks = pem::parse_many(bytes)
                .map_err(|error| format!("failed to parse NODE_EXTRA_CA_CERTS: {error}"))?;
            let certs = blocks
                .into_iter()
                .filter(|block| block.tag() == "CERTIFICATE")
                .map(|block| CertificateDer::from(block.into_contents()));
            let (added, ignored) = roots.add_parsable_certificates(certs);
            tracing::debug!(
                "CA certs: Appended {added} extra certificates for WebSocket ({ignored} ignored)"
            );
        }

        let identity = match (&self.client_cert, &self.client_key) {
            (None, None) => None,
            (Some(_), None) | (None, Some(_)) => {
                return Err(
                    "both LINGXI_CLIENT_CERT and LINGXI_CLIENT_KEY are required for mTLS"
                        .to_string(),
                );
            }
            (Some(cert_path), Some(key_path)) => {
                let cert_pem = read_pem(cert_path, "client certificate from LINGXI_CLIENT_CERT")
                    .ok_or_else(|| "failed to load WebSocket client certificate".to_string())?;
                let certs: Vec<CertificateDer<'static>> = pem::parse_many(cert_pem.as_bytes())
                    .map_err(|error| format!("failed to parse WebSocket client cert: {error}"))?
                    .into_iter()
                    .filter(|block| block.tag() == "CERTIFICATE")
                    .map(|block| CertificateDer::from(block.into_contents()))
                    .collect();
                if certs.is_empty() {
                    return Err("WebSocket client certificate PEM contains no certificate".into());
                }

                let key_pem = read_pem(key_path, "client key from LINGXI_CLIENT_KEY")
                    .and_then(|raw| {
                        decrypt_key_if_needed(raw, self.client_key_passphrase.as_deref())
                    })
                    .ok_or_else(|| "failed to load WebSocket client private key".to_string())?;
                let block = pem::parse_many(key_pem.as_bytes())
                    .map_err(|error| format!("failed to parse WebSocket client key: {error}"))?
                    .into_iter()
                    .find(|block| {
                        matches!(
                            block.tag(),
                            "PRIVATE KEY" | "RSA PRIVATE KEY" | "EC PRIVATE KEY"
                        )
                    })
                    .ok_or_else(|| {
                        "WebSocket client key PEM contains no supported private key".to_string()
                    })?;
                let tag = block.tag().to_string();
                let bytes = block.into_contents();
                let key = match tag.as_str() {
                    "PRIVATE KEY" => PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(bytes)),
                    "RSA PRIVATE KEY" => PrivateKeyDer::Pkcs1(PrivatePkcs1KeyDer::from(bytes)),
                    "EC PRIVATE KEY" => PrivateKeyDer::Sec1(PrivateSec1KeyDer::from(bytes)),
                    _ => unreachable!("tag filtered above"),
                };
                Some((certs, key))
            }
        };

        let builder = rustls::ClientConfig::builder().with_root_certificates(roots);
        let config = match identity {
            Some((certs, key)) => builder
                .with_client_auth_cert(certs, key)
                .map_err(|error| format!("failed to build WebSocket mTLS identity: {error}"))?,
            None => builder.with_no_client_auth(),
        };
        Ok(Arc::new(config))
    }

    /// Build the reqwest client identity from `client_cert` + `client_key`
    /// (`k5()`). Returns `None` when either path is absent or unreadable.
    fn client_identity(&self) -> Option<reqwest::Identity> {
        let cert_path = self.client_cert.as_ref()?;
        let key_path = self.client_key.as_ref()?;

        let cert_pem = read_pem(cert_path, "client certificate from LINGXI_CLIENT_CERT")?;
        let key_pem_raw = read_pem(key_path, "client key from LINGXI_CLIENT_KEY")?;
        let key_pem = decrypt_key_if_needed(key_pem_raw, self.client_key_passphrase.as_deref())?;

        // rustls' `Identity::from_pem` wants the certificate chain and the
        // (unencrypted) private key concatenated into one PEM buffer.
        let mut bundle = cert_pem.into_bytes();
        if !bundle.ends_with(b"\n") {
            bundle.push(b'\n');
        }
        bundle.extend_from_slice(key_pem.as_bytes());

        match reqwest::Identity::from_pem(&bundle) {
            Ok(identity) => Some(identity),
            Err(err) => {
                tracing::error!("mTLS: Failed to build client identity: {err}");
                None
            }
        }
    }
}

/// Read a PEM file, logging the byte-faithful `mTLS: Loaded …` /
/// `mTLS: Failed to load …` lines (`zsl()`).
fn read_pem(path: &Path, label: &str) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(content) => {
            tracing::debug!("mTLS: Loaded {label}");
            Some(content)
        }
        Err(err) => {
            tracing::error!("mTLS: Failed to load {label}: {err}");
            None
        }
    }
}

/// When a passphrase is configured and the key PEM is an `ENCRYPTED PRIVATE KEY`
/// (PKCS#8, PBES2), decrypt it to an unencrypted `PRIVATE KEY` PEM so rustls can
/// consume it. Node performs this natively via the `passphrase` agent option.
///
/// A passphrase set against an already-unencrypted key is ignored (matching
/// Node, which ignores `passphrase` when the key is not encrypted).
fn decrypt_key_if_needed(key_pem: String, passphrase: Option<&str>) -> Option<String> {
    let Some(passphrase) = passphrase else {
        return Some(key_pem);
    };
    // `k5()` logs this whenever the passphrase env var is set.
    tracing::debug!("mTLS: Using client key passphrase");

    let blocks = match pem::parse_many(key_pem.as_bytes()) {
        Ok(blocks) => blocks,
        Err(err) => {
            tracing::error!("mTLS: Failed to parse client key PEM: {err}");
            return Some(key_pem);
        }
    };
    let Some(encrypted) = blocks.iter().find(|b| b.tag() == "ENCRYPTED PRIVATE KEY") else {
        // Passphrase configured but the key is not encrypted — use as-is.
        return Some(key_pem);
    };

    let epki = match pkcs8::EncryptedPrivateKeyInfo::try_from(encrypted.contents()) {
        Ok(epki) => epki,
        Err(err) => {
            tracing::error!("mTLS: Failed to parse encrypted client key: {err}");
            return None;
        }
    };
    match epki.decrypt(passphrase.as_bytes()) {
        Ok(doc) => {
            let block = pem::Pem::new("PRIVATE KEY", doc.as_bytes().to_vec());
            Some(pem::encode(&block))
        }
        Err(err) => {
            tracing::error!("mTLS: Failed to decrypt client key with passphrase: {err}");
            None
        }
    }
}

/// Add the host OS trust store to the builder (`zz` `system` source).
fn add_system_roots(mut builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    let loaded = rustls_native_certs::load_native_certs();
    if !loaded.errors.is_empty() {
        tracing::warn!(
            "CA certs: {} system root(s) failed to load: {:?}",
            loaded.errors.len(),
            loaded.errors
        );
    }
    let mut count = 0usize;
    for der in loaded.certs {
        // Ignore individual malformed roots; a partial system store is fine.
        if let Ok(cert) = reqwest::Certificate::from_der(&der) {
            builder = builder.add_root_certificate(cert);
            count += 1;
        }
    }
    tracing::debug!("CA certs: Loaded {count} system CA certificates");
    builder
}

/// Append `NODE_EXTRA_CA_CERTS` to the builder (`zz` extra-certs branch).
fn add_extra_ca_certs(mut builder: reqwest::ClientBuilder, path: &Path) -> reqwest::ClientBuilder {
    match std::fs::read(path) {
        Ok(bytes) => match reqwest::Certificate::from_pem_bundle(&bytes) {
            Ok(certs) => {
                for cert in certs {
                    builder = builder.add_root_certificate(cert);
                }
                tracing::debug!(
                    "CA certs: Appended extra certificates from NODE_EXTRA_CA_CERTS ({})",
                    path.display()
                );
            }
            Err(err) => {
                tracing::error!(
                    "CA certs: Failed to parse NODE_EXTRA_CA_CERTS file ({}): {err}",
                    path.display()
                );
            }
        },
        Err(err) => {
            tracing::error!(
                "CA certs: Failed to read NODE_EXTRA_CA_CERTS file ({}): {err}",
                path.display()
            );
        }
    }
    builder
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_store_defaults_when_unset_or_empty() {
        assert_eq!(
            parse_cert_store(None),
            vec![CertSource::Bundled, CertSource::System]
        );
        assert_eq!(
            parse_cert_store(Some("")),
            vec![CertSource::Bundled, CertSource::System]
        );
    }

    #[test]
    fn cert_store_parses_and_dedupes_in_order() {
        assert_eq!(parse_cert_store(Some("system")), vec![CertSource::System]);
        assert_eq!(
            parse_cert_store(Some("  SYSTEM , Bundled ")),
            vec![CertSource::System, CertSource::Bundled],
            "trimmed + case-insensitive, first-seen order preserved"
        );
        assert_eq!(
            parse_cert_store(Some("bundled,bundled,system,system")),
            vec![CertSource::Bundled, CertSource::System],
            "duplicates collapse"
        );
    }

    #[test]
    fn cert_store_unrecognized_tokens_ignored_valid_kept() {
        // Unknown token is dropped; the valid one survives.
        assert_eq!(
            parse_cert_store(Some("bogus,system")),
            vec![CertSource::System]
        );
    }

    #[test]
    fn cert_store_all_invalid_falls_back_to_default() {
        assert_eq!(
            parse_cert_store(Some("bogus,,nonsense")),
            vec![CertSource::Bundled, CertSource::System]
        );
    }

    /// Empty settings apply cleanly (no identity, bundled roots kept) and build
    /// a working client — the zero-config default path must not regress.
    #[test]
    fn empty_settings_build_ok() {
        let settings = TlsSettings::default();
        assert!(!settings.has_client_identity());
        let client = settings
            .apply_to_builder(reqwest::Client::builder())
            .build();
        assert!(
            client.is_ok(),
            "default TLS settings must build: {client:?}"
        );
    }

    /// A passphrase-protected PKCS#8 key (openssl `-topk8`, PBES2/PBKDF2/
    /// AES-256-CBC) decrypts and combines with its certificate into a valid
    /// reqwest identity. Fixtures generated offline; passphrase = `hunter2`.
    #[test]
    fn encrypted_pkcs8_key_decrypts_into_identity() {
        let decrypted = decrypt_key_if_needed(ENC_KEY_PEM.to_string(), Some("hunter2"))
            .expect("decryption must succeed");
        assert!(
            decrypted.contains("BEGIN PRIVATE KEY"),
            "decrypted key must be an unencrypted PKCS#8 PEM"
        );
        let bundle = format!("{CERT_PEM}\n{decrypted}");
        let identity = reqwest::Identity::from_pem(bundle.as_bytes());
        assert!(
            identity.is_ok(),
            "cert + decrypted key must form an identity: {identity:?}"
        );
    }

    /// The wrong passphrase fails decryption and yields no key (no panic, no
    /// silent success).
    #[test]
    fn encrypted_pkcs8_key_wrong_passphrase_fails() {
        assert!(decrypt_key_if_needed(ENC_KEY_PEM.to_string(), Some("wrong")).is_none());
    }

    /// `from_env` reads the rebranded `LINGXI_*` spelling first and falls back
    /// to the upstream `CLAUDE_CODE_*` spelling. Exercised on the string-typed
    /// fields (`cert_store`, `passphrase`) so no file I/O runs in the shared-env
    /// window; wrapped in a mutex so it can't race other env-reading tests.
    #[test]
    fn from_env_prefers_lingxi_then_falls_back_to_claude_code() {
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();

        for k in [
            "LINGXI_CERT_STORE",
            "CLAUDE_CODE_CERT_STORE",
            "LINGXI_CLIENT_KEY_PASSPHRASE",
            "CLAUDE_CODE_CLIENT_KEY_PASSPHRASE",
        ] {
            std::env::remove_var(k);
        }

        // Fallback: only the CLAUDE_CODE_ spelling is set.
        std::env::set_var("CLAUDE_CODE_CERT_STORE", "system");
        std::env::set_var("CLAUDE_CODE_CLIENT_KEY_PASSPHRASE", "cc-pass");
        let s = TlsSettings::from_env();
        assert_eq!(s.cert_store.as_deref(), Some("system"));
        assert_eq!(s.client_key_passphrase.as_deref(), Some("cc-pass"));

        // Precedence: the LINGXI_ spelling wins when both are set.
        std::env::set_var("LINGXI_CERT_STORE", "bundled");
        std::env::set_var("LINGXI_CLIENT_KEY_PASSPHRASE", "lx-pass");
        let s = TlsSettings::from_env();
        assert_eq!(s.cert_store.as_deref(), Some("bundled"));
        assert_eq!(s.client_key_passphrase.as_deref(), Some("lx-pass"));

        for k in [
            "LINGXI_CERT_STORE",
            "CLAUDE_CODE_CERT_STORE",
            "LINGXI_CLIENT_KEY_PASSPHRASE",
            "CLAUDE_CODE_CLIENT_KEY_PASSPHRASE",
        ] {
            std::env::remove_var(k);
        }
    }

    /// End-to-end mutual-TLS: a loopback HTTPS server that *requires* a
    /// client certificate accepts a reqwest client built through
    /// [`TlsSettings::apply_to_builder`] with a client identity (proving the
    /// identity is applied to the built client), and *rejects* the same client
    /// built without one (proving the handshake genuinely enforces mTLS). The
    /// custom test CA is trusted via the `NODE_EXTRA_CA_CERTS` path.
    #[tokio::test]
    async fn mtls_identity_is_applied_and_required() {
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
        use tokio_rustls::rustls::server::WebPkiClientVerifier;
        use tokio_rustls::rustls::{RootCertStore, ServerConfig};
        use tokio_rustls::TlsAcceptor;

        // --- Mint a CA + server leaf + client leaf (all CA-signed). ---
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "lingxi-mtls-ca");
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        let host = "lingxi-mtls.test";
        let srv_key = rcgen::KeyPair::generate().unwrap();
        let srv_params = rcgen::CertificateParams::new(vec![host.to_string()]).unwrap();
        let srv_cert = srv_params.signed_by(&srv_key, &ca_cert, &ca_key).unwrap();

        let mut cli_params =
            rcgen::CertificateParams::new(vec!["lingxi-mtls-client".to_string()]).unwrap();
        cli_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
        let cli_key = rcgen::KeyPair::generate().unwrap();
        let cli_cert = cli_params.signed_by(&cli_key, &ca_cert, &ca_key).unwrap();

        // --- Client-auth-required rustls server on loopback. ---
        let ca_der = CertificateDer::from(ca_cert.der().to_vec());
        let mut client_roots = RootCertStore::empty();
        client_roots.add(ca_der.clone()).unwrap();
        let verifier = WebPkiClientVerifier::builder(Arc::new(client_roots))
            .build()
            .unwrap();
        let srv_cert_der = CertificateDer::from(srv_cert.der().to_vec());
        let srv_key_der = PrivateKeyDer::try_from(srv_key.serialize_der()).unwrap();
        let mut config = ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(vec![srv_cert_der], srv_key_der)
            .unwrap();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = TlsAcceptor::from(Arc::new(config));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    break;
                };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let mut buf = [0u8; 1024];
                    let _ = tls.read(&mut buf).await;
                    let _ = tls
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                        )
                        .await;
                    let _ = tls.flush().await;
                    let _ = tls.shutdown().await;
                });
            }
        });

        // --- Write the client identity + CA trust to PEM files. ---
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("client-cert.pem");
        let key_path = dir.path().join("client-key.pem");
        let ca_path = dir.path().join("ca.pem");
        std::fs::write(&cert_path, cli_cert.pem()).unwrap();
        std::fs::write(&key_path, cli_key.serialize_pem()).unwrap();
        std::fs::write(&ca_path, ca_cert.pem()).unwrap();

        let url = format!("https://{host}:{port}/");

        // With identity → handshake completes, 200 "ok".
        let with_id = TlsSettings {
            client_cert: Some(cert_path.clone()),
            client_key: Some(key_path.clone()),
            client_key_passphrase: None,
            cert_store: None,
            extra_ca_certs: Some(ca_path.clone()),
        };
        let client = with_id
            .apply_to_builder(reqwest::Client::builder().resolve(host, addr))
            .build()
            .unwrap();
        let resp = client
            .get(&url)
            .send()
            .await
            .expect("mTLS request must connect");
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(resp.text().await.unwrap(), "ok");

        // Provider traffic delegates to the SDK but retains this same CA and
        // client identity through its constrained builder configurator.
        use futures_util::StreamExt;
        use lingxi_llm_client::Transport as _;
        let provider = lingxi_llm_client::HttpTransport::with_client_configurator(|builder| {
            with_id.apply_to_builder(builder.resolve(host, addr))
        })
        .unwrap();
        let mut response = provider
            .send_stream(lingxi_llm_client::HttpStreamRequest {
                method: "POST".into(),
                url: url.clone(),
                headers: vec![],
                body: futures_util::stream::empty().boxed(),
                content_length: 0,
                timeout: Some(std::time::Duration::from_secs(5)),
            })
            .await
            .expect("SDK upload must inherit custom CA and mTLS identity");
        assert_eq!(response.status, 200);
        assert_eq!(response.body.next().await.unwrap().unwrap(), "ok");

        // Without identity → the server rejects the handshake.
        let without_id = TlsSettings {
            client_cert: None,
            client_key: None,
            client_key_passphrase: None,
            cert_store: None,
            extra_ca_certs: Some(ca_path.clone()),
        };
        let client = without_id
            .apply_to_builder(reqwest::Client::builder().resolve(host, addr))
            .build()
            .unwrap();
        let resp = client.get(&url).send().await;
        assert!(
            resp.is_err(),
            "server requires a client cert; the identity-less client must fail, got: {resp:?}"
        );
        let provider = lingxi_llm_client::HttpTransport::with_client_configurator(|builder| {
            without_id.apply_to_builder(builder.resolve(host, addr))
        })
        .unwrap();
        let result = provider
            .send_stream(lingxi_llm_client::HttpStreamRequest {
                method: "POST".into(),
                url,
                headers: vec![],
                body: futures_util::stream::empty().boxed(),
                content_length: 0,
                timeout: Some(std::time::Duration::from_secs(5)),
            })
            .await;
        assert!(
            result.is_err(),
            "SDK must not bypass the server's mTLS requirement"
        );
    }

    /// The WebSocket connector must inherit both custom CA trust and the mTLS
    /// identity. This exercises a real WSS upgrade rather than inspecting the
    /// constructed rustls config.
    #[tokio::test]
    async fn websocket_connector_applies_custom_ca_and_client_identity() {
        use std::sync::Arc;
        use tokio::net::TcpListener;
        use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
        use tokio_rustls::rustls::server::WebPkiClientVerifier;
        use tokio_rustls::rustls::{RootCertStore, ServerConfig};
        use tokio_rustls::TlsAcceptor;

        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        let server_key = rcgen::KeyPair::generate().unwrap();
        let server_params = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()]).unwrap();
        let server_cert = server_params
            .signed_by(&server_key, &ca_cert, &ca_key)
            .unwrap();

        let client_key = rcgen::KeyPair::generate().unwrap();
        let mut client_params =
            rcgen::CertificateParams::new(vec!["lingxi-wss-client".to_string()]).unwrap();
        client_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
        let client_cert = client_params
            .signed_by(&client_key, &ca_cert, &ca_key)
            .unwrap();

        let mut client_roots = RootCertStore::empty();
        client_roots
            .add(CertificateDer::from(ca_cert.der().to_vec()))
            .unwrap();
        let verifier = WebPkiClientVerifier::builder(Arc::new(client_roots))
            .build()
            .unwrap();
        let server_config = ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(
                vec![CertificateDer::from(server_cert.der().to_vec())],
                PrivateKeyDer::try_from(server_key.serialize_der()).unwrap(),
            )
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..5 {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let Ok(mut websocket) = tokio_tungstenite::accept_async(tls).await else {
                        return;
                    };
                    let _ = futures_util::SinkExt::close(&mut websocket).await;
                });
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("client-cert.pem");
        let key_path = dir.path().join("client-key.pem");
        let ca_path = dir.path().join("ca.pem");
        std::fs::write(&cert_path, client_cert.pem()).unwrap();
        std::fs::write(&key_path, client_key.serialize_pem()).unwrap();
        std::fs::write(&ca_path, ca_cert.pem()).unwrap();

        let settings = TlsSettings {
            client_cert: Some(cert_path),
            client_key: Some(key_path),
            client_key_passphrase: None,
            cert_store: Some("bundled".to_string()),
            extra_ca_certs: Some(ca_path.clone()),
        };
        use lingxi_llm_client::Transport;
        let url = format!("wss://127.0.0.1:{}/", addr.port());
        let transport = lingxi_llm_client::HttpTransport::with_client_configurator(|builder| {
            settings.apply_to_builder(builder)
        })
        .unwrap();
        let request = || lingxi_llm_client::HttpRequest {
            method: "GET".into(),
            url: url.clone(),
            headers: vec![],
            body: Default::default(),
            timeout: Some(std::time::Duration::from_secs(2)),
        };
        assert!(transport.connect_websocket(request()).await.is_ok());
        {
            use lingxi_llm_client::realtime::RealtimeTransport;
            assert!(transport
                .connect(lingxi_llm_client::realtime::RealtimeConnectRequest {
                    endpoint: url.clone(),
                    headers: vec![],
                    max_frame_bytes: 4096
                })
                .await
                .is_ok());
        }
        let untrusted = lingxi_llm_client::HttpTransport::new().unwrap();
        assert!(matches!(
            untrusted.connect_websocket(request()).await,
            Err(lingxi_llm_client::protocol::LlmError::TlsCert { .. })
        ));

        {
            use lingxi_llm_client::realtime::{RealtimeError, RealtimeTransport};
            assert!(matches!(
                untrusted
                    .connect(lingxi_llm_client::realtime::RealtimeConnectRequest {
                        endpoint: url.clone(),
                        headers: vec![],
                        max_frame_bytes: 4096
                    })
                    .await,
                Err(RealtimeError::TlsCert { .. })
            ));
        }
        let without_identity = TlsSettings {
            client_cert: None,
            client_key: None,
            client_key_passphrase: None,
            cert_store: Some("bundled".to_string()),
            extra_ca_certs: Some(ca_path),
        };
        let transport = lingxi_llm_client::HttpTransport::with_client_configurator(|builder| {
            without_identity.apply_to_builder(builder)
        })
        .unwrap();
        assert!(
            transport.connect_websocket(request()).await.is_err(),
            "mTLS must reject a missing identity"
        );
    }

    const CERT_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIDFzCCAf+gAwIBAgIUD8Y6Rg3Xhlznbi0NlvIZp9VcH0AwDQYJKoZIhvcNAQEL
BQAwGzEZMBcGA1UEAwwQbGluZ3hpLW10bHMtdGVzdDAeFw0yNjA3MTMxNzU2MjFa
Fw0zNjA3MTAxNzU2MjFaMBsxGTAXBgNVBAMMEGxpbmd4aS1tdGxzLXRlc3QwggEi
MA0GCSqGSIb3DQEBAQUAA4IBDwAwggEKAoIBAQDeRmjB4gzd9lRizloVlzKmoMnp
cBb7/kHXpIdy7DzoR8ROWOIwyd/wfI9m6nkRTliWpYzLgk2xJwr/hteip+kSAAKj
ARVysilA/ogijRO4/b9c4mFk7BVj6lhQnd4aUJ5LyJIhgVa0ZtHxOY+GG4G6ng9n
uu9VRik3RIS9M1avG6LxWL1yI2ItlXfMXYr50LHQfnal20U/y7DhJpedilopskEk
t6ijUpdB6aO28bj/i+Mp67ZMDPOLqJUKwGOVc/XaU20FyBk3+0+CKjlMXk2XNCrw
uGxrgVE9peh4Lm74nsQWvzBO/MizcArYt4arxMcfl2Y9MLLrAzRnGHkD1oohAgMB
AAGjUzBRMB0GA1UdDgQWBBSIXMKwI7cm+llnKqVB0aItoXxCezAfBgNVHSMEGDAW
gBSIXMKwI7cm+llnKqVB0aItoXxCezAPBgNVHRMBAf8EBTADAQH/MA0GCSqGSIb3
DQEBCwUAA4IBAQBqMeEwgfBsTJ15CvUFt9+6g1KYt7z/6XnYsam1lHmJ9YfbaurM
qCjEcsAVXEWlkpmDLStvEvNVGTIBV4ROzsy1V6EjTdkQrFB6DvwK1rzZ+vaq3IMG
upnTIX+iE0qiVNlVkeEkOm31+dE6cyqe7yZKtHmBu7bsiMKkBqQGSjsDt7rCnU2I
KyTw87iY/YwlmWi0E42MC7JvG1cp/nTBAj626fj75pcWpgsRYdA0ewrZgNaVcmrb
o3VMGyw9N8lx74vxyLBCd5I0miamc/7AwyOFZEbn7uYqRJyku4c+RVBf+MoBWGy7
GWgH8jj6ZOGVZoHHW9HElklLqxhNhpHgXT9U
-----END CERTIFICATE-----";

    const ENC_KEY_PEM: &str = "-----BEGIN ENCRYPTED PRIVATE KEY-----
MIIFLTBXBgkqhkiG9w0BBQ0wSjApBgkqhkiG9w0BBQwwHAQIIy8dOiFIYIwCAggA
MAwGCCqGSIb3DQIJBQAwHQYJYIZIAWUDBAEqBBB0Wdk5exjc+vJuOY3VMDZcBIIE
0NIzrrfuiGDEcd8G5L51VnfV+e3RrVob6q0lI9NCrhKvL6ab6GNWXgnLPWHtKo2s
Ci05GZhGEaOpIYpXiWmiWtZsRAyFy7sDziSbcQC08hS+YSg43dMZJXfzAYyLAtxg
F2xuHE64x5hTevX4tufN/sKhgkWT5m+dljLjFeKVrzeppqPtQbsv6nQoiyEQfA3O
5RUAErHwD1ni3VHPvVH5GL0rng/PBt3O7DoQI1dmGJgrzUZWpqJvZx5IHQpfVFN9
NvJHUtRY3jBxo9YyocB5C+FAdoxIf4MRG2fDGQa4p/2UZnOIhgvbin6RaTv/IZry
dU+fLYK5+iP368YOSDQlVQKIipswVwcMvzH1L/RSVLD2YPUZ1I4KsyieRG/7wYzw
S+Gh43pnKpMmI/jcTHR2fm4fin8tFGh1p3HmKFXyjNF331Pr8OJJoeI8vRaHMBV/
nNZCSOYdx/hw0sOCuX2DoU9vZBtJrIXKTgUkiOwtHWnsetfCUGk/XelF7YqdjTxR
FlFJPOo+aYMWxkOu4nVg16DLZci/RohHeoWv7s78Rd6DnYfkyVv63Zt2Wwb8mTPL
QjXbKx0d4d8KSLI/EJAa6r/yniO4uIt6bLTOAJf8ZztrI5Mvyoz1UHqSupJq0Or0
/Raw3nkqN1dSsA8VLf7WgZ8EaLdnlDnwQucpiXRicPbRTdWdXuPgun5zseT4ufpE
tl1BbE4rOgRrFDHhKxOcesJw7YJVEK9dcRUhMnl/y64w2uwnY7S3rs0ts/FyLPvF
Ae932PgTE8ZtfCb4Id3OMn64lGGkN4E9STCxOhoeZnNxf5Bdadtc31v01ojq1cD3
ib286Q2GOVTnp0CAvuKo8aqHKFpmzHVLiwtcZAN3nLpG7p7sN3Zibax7Y9BinaRB
T/RMHWwgQDOqyPP6v4NmQBHvlw1siz+gGteU6RNTz164ytVIXgB2YEZZFCEVLqag
wRbMQ5Izi53hICoqbazhHGeOiB7tu63aAjgt2AqVrYxAGeiwVHTOxGK+Bqg1kML4
Mtn0lJqWyPZogH6iHZSs+jUfCbTko5zd9mqZesRfIpfKnSKUM7dcVJMFoefL/JMh
Q+3Cr7fGPUaLKdLS7tIVfgZBbF7X0e4+nryxslyGKJibAqdCSg+a/orcFuhcpac9
LZXN07iBh83K8PMCiF9F9xQI/WY1KcgVtmDYbIOipNuSXAMe/yOPihp8Kys4qZqK
IUNDb+fJdA995NTvJMYl5fZ/9pI4+R41oTjH2Z3TZQJpdtHPD4oy9WGIq5wSOnhU
4lMYBQJYNHneumZ+uaXVwRK2urqPo/XKIEEPZnIYBK1WsMHT/VG+EInvxKoMjXJk
d6adD8j7W5jO9pYMP9CrDJTRr+t9DgK3PpDv1ORGX1Iy6BG0N8v8AwsiA1dyyivA
u9WGG3HH0yYyyTQtlM02zeScttZ/XU8UbHxpYyHyGKi5TbzSDZdiEK6+eVPgf/e5
hezXiqQ8Rk5K7I86ZzUVRp6ui1lOL2UFM6ZCSRIWngO6XEO7gDTiPnedbRFw+rrB
CxL7nIwhlU1xmEvk7YW+0nFCol+b6gNvMwOvRY+88HJJ+qgT11EfQHys0WqWf4EH
8N0sDA26DU4eyZmE+1fp3ieWAHb4HfyefhorOI+DdfCf
-----END ENCRYPTED PRIVATE KEY-----";
}
