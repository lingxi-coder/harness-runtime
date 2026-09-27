//! MITM CA loader/generator for the in-process TLS-terminating proxy.
//!
//! Faithful 1:1 port of `mitm-ca.js`. The CA is supplied via
//! `network.tlsTerminate.{caCertPath,caKeyPath}`. If both paths are omitted,
//! an ephemeral RSA-2048 self-signed CA is generated into a temp directory; the
//! cert path is what the trust env vars point at. The caller is responsible for
//! cleaning up via [`dispose_mitm_ca`] (`SandboxManager::reset()` does this).
//!
//! ## Crypto backend
//!
//! The TS uses `node-forge`. The faithful Rust equivalent is [`rcgen`] (X.509
//! building/signing, kept on the **ring** backend to match rustls 0.22's
//! provider — no `aws-lc-rs` C build) plus the [`rsa`] crate for RSA-2048
//! keygen (ring can sign RSA but cannot generate RSA keys). The CA is RSA-2048,
//! SHA-256, validity `[now-1d, now+825d]`, `basicConstraints cA:true critical`,
//! `keyUsage critical {keyCertSign,cRLSign,digitalSignature}`, plus a
//! subjectKeyIdentifier — exactly as the TS emits.
//!
//! ## Interior mutability
//!
//! The TS uses plain `Map`s for the leaf/secure-context caches. The Rust proxy
//! shares one [`MitmCa`] across async tasks, so the caches are wrapped in
//! [`Mutex`] (see [`MitmCa::leaf_certs`] / [`MitmCa::server_configs`]).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose,
    SerialNumber, PKCS_RSA_SHA256,
};
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use rsa::RsaPrivateKey;
use time::{Duration, OffsetDateTime};

/// A minted per-host leaf certificate plus its private key, both PEM-encoded.
///
/// `cert_pem` is the leaf certificate concatenated with the CA certificate (the
/// full chain), so a TLS server presenting it sends both to the client.
#[derive(Clone, Debug)]
pub struct Leaf {
    /// Leaf certificate PEM concatenated with the CA certificate PEM (chain).
    pub cert_pem: String,
    /// The leaf private key, PKCS#8 PEM-encoded.
    pub key_pem: String,
}

/// Options controlling [`create_mitm_ca`]: either both paths are set (load a
/// user-supplied CA) or both omitted (generate an ephemeral CA).
#[derive(Clone, Debug, Default)]
pub struct MitmCaOptions {
    /// Path to a user-supplied CA certificate PEM. Must be set together with
    /// [`Self::ca_key_path`].
    pub ca_cert_path: Option<PathBuf>,
    /// Path to a user-supplied CA private key PEM (must be RSA). Must be set
    /// together with [`Self::ca_cert_path`].
    pub ca_key_path: Option<PathBuf>,
}

/// A loaded or generated MITM certificate authority plus its per-host caches.
///
/// Owns the rcgen signing material (`signing_key` + `ca_cert`) used to mint leaf
/// certificates, the CA's own PEMs, and the on-disk paths the trust env vars
/// point at.
pub struct MitmCa {
    /// On-disk path of the CA certificate PEM (`ca.crt` for ephemeral CAs).
    pub cert_path: PathBuf,
    /// On-disk path of the CA private key PEM (`ca.key` for ephemeral CAs).
    pub key_path: PathBuf,
    /// The CA certificate, PEM-encoded.
    pub cert_pem: String,
    /// The CA private key, PEM-encoded.
    pub key_pem: String,
    /// `true` if this CA was generated into a temp directory we own (so
    /// [`dispose_mitm_ca`] removes it); `false` for user-supplied CAs.
    pub ephemeral: bool,
    /// The rcgen CA certificate used as the issuer when minting leaves. Its
    /// subject DN + key-identifier method are what [`rcgen::CertificateParams::signed_by`]
    /// copies into each leaf's issuer field. For a loaded user CA this is an
    /// rcgen re-issuance of the on-disk cert built from its parsed params (same
    /// subject DN + SKI, so leaf chains still verify against the on-disk CA);
    /// the on-disk PEM is preserved verbatim in [`Self::cert_pem`].
    pub(crate) ca_cert: Certificate,
    /// The rcgen signing key (wraps the RSA key with the SHA-256 algorithm).
    pub(crate) signing_key: KeyPair,
    /// Per-host minted-leaf cache (TS `ca.leafCerts`). Keyed by hostname.
    pub(crate) leaf_certs: Mutex<HashMap<String, Leaf>>,
    /// Per-host `rustls::ServerConfig` cache (the Rust analogue of the TS
    /// `ca.secureContexts` `SNICallback` cache). Keyed by hostname.
    pub(crate) server_configs: Mutex<HashMap<String, Arc<rustls::ServerConfig>>>,
    /// Per-host `rustls::sign::CertifiedKey` cache, keyed by hostname. This is
    /// the SNI-resolution cache for the P6b `MitmCertResolver` (the Rust
    /// analogue of node `tls.createServer`'s `SNICallback` secure-context map):
    /// one minted leaf chain + signing key per host, resolved live from the
    /// `ClientHello` SNI.
    pub(crate) cert_keys: Mutex<HashMap<String, Arc<rustls::sign::CertifiedKey>>>,
}

impl std::fmt::Debug for MitmCa {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MitmCa")
            .field("cert_path", &self.cert_path)
            .field("key_path", &self.key_path)
            .field("ephemeral", &self.ephemeral)
            .finish_non_exhaustive()
    }
}

/// Errors from [`create_mitm_ca`].
#[derive(Debug)]
pub enum MitmCaError {
    /// Exactly one of `caCertPath`/`caKeyPath` was provided. The message is the
    /// verbatim TS string `tlsTerminate: caCertPath and caKeyPath must be
    /// provided together`.
    OnlyOnePath,
    /// A user-supplied CA file could not be read, was not PEM, failed to parse,
    /// or the key was not RSA. Carries the formatted message.
    Load(String),
    /// Key generation, signing, or temp-dir/file I/O failed while generating an
    /// ephemeral CA.
    Generate(String),
}

impl std::fmt::Display for MitmCaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OnlyOnePath => {
                f.write_str("tlsTerminate: caCertPath and caKeyPath must be provided together")
            }
            Self::Load(msg) | Self::Generate(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for MitmCaError {}

/// Create a [`MitmCa`].
///
/// - Both `caCertPath`/`caKeyPath` set → load from disk (errors if either file
///   is missing, unreadable, not PEM, fails to parse, or the key is not RSA).
/// - Exactly one set → [`MitmCaError::OnlyOnePath`] with the verbatim TS message.
/// - Neither set → generate an ephemeral RSA-2048 CA into a fresh `0700` temp dir.
///
/// Pure factory: no module-level state. The caller owns the returned object and
/// its lifetime.
///
/// # Errors
///
/// Returns [`MitmCaError`] on the load/one-path/generate failures described above.
pub fn create_mitm_ca(opts: MitmCaOptions) -> Result<MitmCa, MitmCaError> {
    match (opts.ca_cert_path, opts.ca_key_path) {
        (Some(cert), Some(key)) => load_ca(&cert, &key),
        (Some(_), None) | (None, Some(_)) => Err(MitmCaError::OnlyOnePath),
        (None, None) => generate_ephemeral_ca(),
    }
}

/// Remove the temp directory for an SRT-generated CA. No-op for user CAs.
///
/// Mirrors the TS `disposeMitmCA`: best-effort `rm -rf` of `dirname(certPath)`;
/// failures are swallowed (logged at warn level in the TS).
pub fn dispose_mitm_ca(ca: &MitmCa) {
    if !ca.ephemeral {
        return;
    }
    if let Some(dir) = ca.cert_path.parent() {
        if let Err(err) = std::fs::remove_dir_all(dir) {
            tracing::warn!(target: "mitm_ca", "[mitm-ca] cleanup failed: {err}");
        }
    }
}

/// Read a PEM file, enforcing it carries a `-----BEGIN [..]LABEL-----` block.
///
/// Mirrors the TS `readPem`: accepts a prefixed variant (e.g. `RSA PRIVATE KEY`)
/// for the key case, and produces the same `field: cannot read …` /
/// `field: … is not a PEM LABEL` messages.
fn read_pem(path: &Path, label: &str, field: &str) -> Result<String, MitmCaError> {
    let pem = std::fs::read_to_string(path).map_err(|err| {
        MitmCaError::Load(format!(
            "{field}: cannot read {} ({})",
            path.display(),
            err.kind()
        ))
    })?;
    // Accept the exact label or a prefixed variant (e.g. "RSA PRIVATE KEY"),
    // mirroring the TS regex `-----BEGIN [A-Z ]*LABEL-----`.
    let prefix = "-----BEGIN ";
    let suffix = format!("{label}-----");
    let has_label = pem.lines().any(|line| {
        line.strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(&suffix))
            .is_some_and(|mid| mid.chars().all(|c| c.is_ascii_uppercase() || c == ' '))
    });
    if !has_label {
        return Err(MitmCaError::Load(format!(
            "{field}: {} is not a PEM {label}",
            path.display()
        )));
    }
    Ok(pem)
}

/// Load a user-supplied CA from cert + key PEM files.
fn load_ca(cert_path: &Path, key_path: &Path) -> Result<MitmCa, MitmCaError> {
    let cert_pem = read_pem(cert_path, "CERTIFICATE", "tlsTerminate.caCertPath")?;
    let key_pem = read_pem(key_path, "PRIVATE KEY", "tlsTerminate.caKeyPath")?;

    // Enforce the key is RSA (the TS checks `'n' in key && 'd' in key` because
    // node-forge can only sign with RSA private keys; rcgen+ring sign RSA the
    // same way). Parsing as an RSA PKCS#8 key both validates and proves RSA-ness.
    let rsa_key = RsaPrivateKey::from_pkcs8_pem(&key_pem).map_err(|_| {
        MitmCaError::Load(format!(
            "tlsTerminate.caKeyPath: CA key at {} must be RSA",
            key_path.display()
        ))
    })?;
    // Re-encode to canonical PKCS#8 PEM so rcgen's KeyPair accepts it (the
    // on-disk form may be PKCS#1 "RSA PRIVATE KEY").
    let pkcs8_pem = rsa_key.to_pkcs8_pem(LineEnding::LF).map_err(|err| {
        MitmCaError::Load(format!(
            "tlsTerminate: failed to parse CA from {}: {err}",
            cert_path.display()
        ))
    })?;
    let signing_key =
        KeyPair::from_pkcs8_pem_and_sign_algo(&pkcs8_pem, &PKCS_RSA_SHA256).map_err(|err| {
            MitmCaError::Load(format!(
                "tlsTerminate: failed to parse CA from {}: {err}",
                cert_path.display()
            ))
        })?;
    let params = CertificateParams::from_ca_cert_pem(&cert_pem).map_err(|err| {
        MitmCaError::Load(format!(
            "tlsTerminate: failed to parse CA from {}: {err}",
            cert_path.display()
        ))
    })?;
    // Re-issue an rcgen Certificate from the parsed params so it can act as the
    // issuer in `signed_by`. Only its subject DN + SKI method are consumed; the
    // on-disk PEM is what we actually ship in the chain (preserved above).
    let ca_cert = params.self_signed(&signing_key).map_err(|err| {
        MitmCaError::Load(format!(
            "tlsTerminate: failed to parse CA from {}: {err}",
            cert_path.display()
        ))
    })?;

    Ok(MitmCa {
        cert_path: cert_path.to_path_buf(),
        key_path: key_path.to_path_buf(),
        cert_pem,
        key_pem,
        ephemeral: false,
        ca_cert,
        signing_key,
        leaf_certs: Mutex::new(HashMap::new()),
        server_configs: Mutex::new(HashMap::new()),
        cert_keys: Mutex::new(HashMap::new()),
    })
}

/// Generate a fresh ephemeral RSA-2048 self-signed CA into a `0700` temp dir.
fn generate_ephemeral_ca() -> Result<MitmCa, MitmCaError> {
    // RSA-2048 keygen via the `rsa` crate (ring cannot generate RSA). `OsRng`
    // is re-exported by `rsa` at the `rand_core` version it expects.
    let mut rng = rsa::rand_core::OsRng;
    let rsa_key = RsaPrivateKey::new(&mut rng, 2048)
        .map_err(|err| MitmCaError::Generate(format!("[mitm-ca] RSA keygen failed: {err}")))?;
    let key_pem = rsa_key
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|err| MitmCaError::Generate(format!("[mitm-ca] key encode failed: {err}")))?
        .to_string();
    let signing_key = KeyPair::from_pkcs8_pem_and_sign_algo(&key_pem, &PKCS_RSA_SHA256)
        .map_err(|err| MitmCaError::Generate(format!("[mitm-ca] key wrap failed: {err}")))?;

    let now = OffsetDateTime::now_utc();
    let mut params = CertificateParams::default();
    params.serial_number = Some(SerialNumber::from(random_serial()));
    params.not_before = now - Duration::days(1);
    params.not_after = now + Duration::days(825);
    // Subject == issuer (self-signed).
    params
        .distinguished_name
        .push(DnType::CommonName, "sandbox-runtime ephemeral CA");
    params
        .distinguished_name
        .push(DnType::OrganizationName, "sandbox-runtime");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    // rcgen emits a subjectKeyIdentifier for CA certs (KeyIdMethod::Sha256 by
    // default) and marks basicConstraints/keyUsage critical, matching the TS.

    let ca_cert: Certificate = params
        .self_signed(&signing_key)
        .map_err(|err| MitmCaError::Generate(format!("[mitm-ca] self-sign failed: {err}")))?;
    let cert_pem = ca_cert.pem();

    // Write to disk so trust env vars can point at a real path. The temp dir is
    // CSPRNG-named + 0700 (mkdtemp parity); ca.crt 0644, ca.key 0600.
    let dir = make_secure_temp_dir()?;
    let cert_path = dir.join("ca.crt");
    let key_path = dir.join("ca.key");
    write_with_mode(&cert_path, cert_pem.as_bytes(), 0o644)?;
    write_with_mode(&key_path, key_pem.as_bytes(), 0o600)?;

    tracing::debug!(target: "mitm_ca", "[mitm-ca] generated ephemeral CA at {}", cert_path.display());

    Ok(MitmCa {
        cert_path,
        key_path,
        cert_pem,
        key_pem,
        ephemeral: true,
        ca_cert,
        signing_key,
        leaf_certs: Mutex::new(HashMap::new()),
        server_configs: Mutex::new(HashMap::new()),
        cert_keys: Mutex::new(HashMap::new()),
    })
}

/// 16 CSPRNG bytes with the high bit cleared so the DER INTEGER stays positive
/// (TS `randomSerial`). Returned as a big-endian byte vector for
/// [`rcgen::SerialNumber`].
fn random_serial() -> Vec<u8> {
    let mut bytes = [0_u8; 16];
    // getrandom is the kernel CSPRNG; on the (extremely rare) failure fall back
    // to a clock/pid mix so we never panic.
    if getrandom::getrandom(&mut bytes).is_err() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let seed =
            now.as_nanos().wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ u128::from(std::process::id());
        bytes.copy_from_slice(&seed.to_be_bytes());
    }
    bytes[0] &= 0x7f;
    bytes.to_vec()
}

/// Create a CSPRNG-named `0700` directory under [`std::env::temp_dir`], the
/// `mkdtemp(tmpdir(), 'srt-ca-')` parity. Unpredictable name + restrictive mode
/// keep the CA private key unreadable by other local users.
fn make_secure_temp_dir() -> Result<PathBuf, MitmCaError> {
    let base = std::env::temp_dir();
    for _ in 0..16 {
        let mut rnd = [0_u8; 8];
        let _ = getrandom::getrandom(&mut rnd);
        let mut suffix = String::with_capacity(16);
        for b in rnd {
            let _ = write!(suffix, "{b:02x}");
        }
        let candidate = base.join(format!("srt-ca-{suffix}"));
        match create_dir_0700(&candidate) {
            Ok(()) => return Ok(candidate),
            // Collision: try another name. Any other error is fatal.
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => {
                return Err(MitmCaError::Generate(format!(
                    "[mitm-ca] mkdtemp failed: {err}"
                )))
            }
        }
    }
    Err(MitmCaError::Generate(
        "[mitm-ca] mkdtemp failed: exhausted attempts".to_string(),
    ))
}

#[cfg(unix)]
fn create_dir_0700(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_dir_0700(path: &Path) -> std::io::Result<()> {
    // No POSIX modes on non-unix; the unguessable name is the protection.
    std::fs::create_dir(path)
}

#[cfg(unix)]
fn write_with_mode(path: &Path, bytes: &[u8], mode: u32) -> Result<(), MitmCaError> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(|err| {
            MitmCaError::Generate(format!("[mitm-ca] write {} failed: {err}", path.display()))
        })?;
    f.write_all(bytes).map_err(|err| {
        MitmCaError::Generate(format!("[mitm-ca] write {} failed: {err}", path.display()))
    })
}

#[cfg(not(unix))]
fn write_with_mode(path: &Path, bytes: &[u8], _mode: u32) -> Result<(), MitmCaError> {
    std::fs::write(path, bytes).map_err(|err| {
        MitmCaError::Generate(format!("[mitm-ca] write {} failed: {err}", path.display()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;
    use x509_parser::prelude::FromDer as _;

    /// Ephemeral CA generates, writes both files with the right modes, the cert
    /// parses as a v3 CA with the right CN, and the temp dir is 0700.
    #[test]
    fn ephemeral_ca_generates_and_writes_files() {
        let ca = create_mitm_ca(MitmCaOptions::default()).expect("generate ephemeral CA");
        assert!(ca.ephemeral);
        assert!(ca.cert_path.ends_with("ca.crt"));
        assert!(ca.key_path.ends_with("ca.key"));

        // Files exist with the right modes.
        let cert_mode = std::fs::metadata(&ca.cert_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let key_mode = std::fs::metadata(&ca.key_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(cert_mode, 0o644, "ca.crt must be 0644");
        assert_eq!(key_mode, 0o600, "ca.key must be 0600");
        let dir_mode = std::fs::metadata(ca.cert_path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "temp dir must be 0700");

        // On-disk PEM matches the in-memory PEM.
        assert_eq!(std::fs::read_to_string(&ca.cert_path).unwrap(), ca.cert_pem);
        assert_eq!(std::fs::read_to_string(&ca.key_path).unwrap(), ca.key_pem);

        // The cert parses as a CA with the right subject CN, RSA-2048 SHA-256.
        let der = x509_parser::pem::parse_x509_pem(ca.cert_pem.as_bytes())
            .unwrap()
            .1;
        let cert = x509_parser::certificate::X509Certificate::from_der(&der.contents)
            .unwrap()
            .1;
        let bc = cert.basic_constraints().unwrap().unwrap();
        assert!(bc.value.ca, "basicConstraints cA must be true");
        assert!(bc.critical, "basicConstraints must be critical");
        let cn = cert
            .subject()
            .iter_common_name()
            .next()
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(cn, "sandbox-runtime ephemeral CA");
        // Self-signed: subject == issuer.
        assert_eq!(cert.subject().to_string(), cert.issuer().to_string());
        // SHA-256 RSA signature (sha256WithRSAEncryption).
        assert_eq!(
            cert.signature_algorithm.oid().to_id_string(),
            "1.2.840.113549.1.1.11"
        );
        // subjectKeyIdentifier present.
        assert!(
            cert.get_extension_unique(
                &x509_parser::oid_registry::OID_X509_EXT_SUBJECT_KEY_IDENTIFIER
            )
            .unwrap()
            .is_some(),
            "CA must carry a subjectKeyIdentifier"
        );

        dispose_mitm_ca(&ca);
        assert!(!ca.cert_path.exists(), "dispose removes the temp dir");
        assert!(!ca.cert_path.parent().unwrap().exists());
    }

    /// Exactly one of cert/key path → the verbatim TS error.
    #[test]
    fn one_path_only_is_exact_error() {
        let err = create_mitm_ca(MitmCaOptions {
            ca_cert_path: Some(PathBuf::from("/tmp/x.crt")),
            ca_key_path: None,
        })
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "tlsTerminate: caCertPath and caKeyPath must be provided together"
        );

        let err = create_mitm_ca(MitmCaOptions {
            ca_cert_path: None,
            ca_key_path: Some(PathBuf::from("/tmp/x.key")),
        })
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "tlsTerminate: caCertPath and caKeyPath must be provided together"
        );
    }

    /// A generated CA round-trips: write its PEMs out, load them back via the
    /// user-CA path, and confirm it parses + is non-ephemeral.
    #[test]
    fn generated_ca_round_trips_through_load() {
        let ca = create_mitm_ca(MitmCaOptions::default()).expect("generate");
        let dir = TempDir::new().unwrap();
        let cert_path = dir.path().join("user.crt");
        let key_path = dir.path().join("user.key");
        std::fs::write(&cert_path, &ca.cert_pem).unwrap();
        std::fs::write(&key_path, &ca.key_pem).unwrap();

        let loaded = create_mitm_ca(MitmCaOptions {
            ca_cert_path: Some(cert_path.clone()),
            ca_key_path: Some(key_path),
        })
        .expect("load user CA");
        assert!(!loaded.ephemeral, "loaded CA is not ephemeral");
        assert_eq!(loaded.cert_path, cert_path);
        assert_eq!(loaded.cert_pem, ca.cert_pem);

        // dispose is a no-op for user CAs (the dir stays).
        dispose_mitm_ca(&loaded);
        assert!(cert_path.exists());

        dispose_mitm_ca(&ca);
    }

    /// Load fails when the cert file is missing / not PEM, and when the key is
    /// not RSA (here: a garbage non-PEM key).
    #[test]
    fn load_rejects_missing_and_non_pem() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("nope.crt");
        let key = dir.path().join("k.key");
        std::fs::write(&key, "-----BEGIN PRIVATE KEY-----\n").unwrap();
        let err = create_mitm_ca(MitmCaOptions {
            ca_cert_path: Some(missing.clone()),
            ca_key_path: Some(key.clone()),
        })
        .unwrap_err();
        assert!(err.to_string().contains("cannot read"), "got: {err}");

        // cert exists but is not PEM.
        let bad_cert = dir.path().join("bad.crt");
        std::fs::write(&bad_cert, "not a pem at all").unwrap();
        let err = create_mitm_ca(MitmCaOptions {
            ca_cert_path: Some(bad_cert),
            ca_key_path: Some(key),
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("is not a PEM CERTIFICATE"),
            "got: {err}"
        );
    }

    /// A non-RSA (EC) CA key is rejected with the "must be RSA" message.
    #[test]
    fn load_rejects_non_rsa_key() {
        let ca = create_mitm_ca(MitmCaOptions::default()).expect("generate");
        let dir = TempDir::new().unwrap();
        let cert_path = dir.path().join("u.crt");
        let key_path = dir.path().join("u.key");
        std::fs::write(&cert_path, &ca.cert_pem).unwrap();
        // A syntactically-valid PKCS#8 PRIVATE KEY PEM that is NOT RSA: generate
        // an EC P-256 key via rcgen and write its PKCS#8 PEM.
        let ec = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        std::fs::write(&key_path, ec.serialize_pem()).unwrap();
        let err = create_mitm_ca(MitmCaOptions {
            ca_cert_path: Some(cert_path),
            ca_key_path: Some(key_path),
        })
        .unwrap_err();
        assert!(err.to_string().contains("must be RSA"), "got: {err}");
        dispose_mitm_ca(&ca);
    }
}
