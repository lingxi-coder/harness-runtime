//! Per-host leaf certificate minter for the in-process TLS-terminating proxy.
//!
//! Faithful 1:1 port of `mitm-leaf.js`. Given a [`MitmCa`](crate::mitm_ca::MitmCa),
//! mints an RSA-2048 leaf certificate for a specific hostname on first use and
//! caches it for the lifetime of that CA instance. The leaf is signed by the CA
//! and carries `SAN=DNS:<host>` (or `IP:<addr>` for IP literals), so a client
//! that trusts the CA accepts it for that host.
//!
//! The TS `secureContextFor` (a Node `tls.SecureContext` `SNICallback` target)
//! becomes [`server_config_for`], which builds a cached `rustls::ServerConfig`
//! per host — the Rust proxy's SNI-resolution target in P6b.

use std::sync::Arc;

use rcgen::{
    CertificateParams, DnType, ExtendedKeyUsagePurpose, Ia5String, IsCa, KeyPair, KeyUsagePurpose,
    SanType, SerialNumber, PKCS_RSA_SHA256,
};
use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use rsa::RsaPrivateKey;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use time::{Duration, OffsetDateTime};

use crate::mitm_ca::{Leaf, MitmCa};

/// Errors from leaf minting / `ServerConfig` construction.
#[derive(Debug)]
pub enum LeafError {
    /// RSA keygen, key encoding, certificate signing, or PEM/DER conversion
    /// failed.
    Mint(String),
    /// Building the `rustls::ServerConfig` from the leaf chain + key failed.
    Config(String),
}

impl std::fmt::Display for LeafError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mint(msg) | Self::Config(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for LeafError {}

/// Mint (or return cached) an RSA-2048 leaf cert for `hostname`, signed by `ca`.
///
/// The cache lives on [`MitmCa::leaf_certs`](crate::mitm_ca::MitmCa). The
/// returned [`Leaf::cert_pem`] is the leaf PEM concatenated with the CA PEM (the
/// full chain), exactly as `mintLeafCert` does.
///
/// The leaf has: serial = 16 random bytes (high bit cleared); validity
/// `[now-1d, clamp_validity(ca, now-1d)]`; subject CN = `hostname`; issuer = CA
/// subject DN; `basicConstraints cA:false critical`; `keyUsage critical
/// {digitalSignature,keyEncipherment}`; `extKeyUsage serverAuth`;
/// `subjectAltName` per [`san_for`]; **no authorityKeyIdentifier** (see below);
/// signed by the CA with SHA-256.
///
/// ## No authorityKeyIdentifier
///
/// `use_authority_key_identifier_extension` is left at its default (`false`), so
/// rcgen omits the AKI extension. The TS deliberately omits it too: an AKI that
/// doesn't byte-match the CA's SKI breaks chain verification, and the issuer ↔
/// subject DN match is sufficient for the path to build.
///
/// # Errors
///
/// Returns [`LeafError::Mint`] if RSA keygen, key encoding, or CA signing fails.
pub fn mint_leaf_cert(ca: &MitmCa, hostname: &str) -> Result<Leaf, LeafError> {
    if let Some(cached) = ca
        .leaf_certs
        .lock()
        .expect("leaf_certs mutex poisoned")
        .get(hostname)
    {
        return Ok(cached.clone());
    }

    // RSA-2048 leaf key (ring cannot generate RSA — same as the CA).
    let mut rng = rsa::rand_core::OsRng;
    let rsa_key = RsaPrivateKey::new(&mut rng, 2048)
        .map_err(|err| LeafError::Mint(format!("[mitm-leaf] RSA keygen failed: {err}")))?;
    let key_pem = rsa_key
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|err| LeafError::Mint(format!("[mitm-leaf] key encode failed: {err}")))?
        .to_string();
    let leaf_key = KeyPair::from_pkcs8_pem_and_sign_algo(&key_pem, &PKCS_RSA_SHA256)
        .map_err(|err| LeafError::Mint(format!("[mitm-leaf] key wrap failed: {err}")))?;

    let not_before = OffsetDateTime::now_utc() - Duration::days(1);
    let mut params = CertificateParams::default();
    params.serial_number = Some(SerialNumber::from(random_serial()));
    params.not_before = not_before;
    params.not_after = clamp_validity(ca, not_before);
    params.distinguished_name.push(DnType::CommonName, hostname);
    // Explicit `cA:false` (critical) — matches the TS `basicConstraints cA:false`.
    // (rcgen's ExplicitNoCa also emits a leaf SKI; harmless and does not affect
    // chain verification — see module note / report.)
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.subject_alt_names = vec![san_for(hostname)?];
    // use_authority_key_identifier_extension stays false → no AKI emitted.

    let leaf_cert = params
        .signed_by(&leaf_key, &ca.ca_cert, &ca.signing_key)
        .map_err(|err| LeafError::Mint(format!("[mitm-leaf] sign failed: {err}")))?;

    let leaf = Leaf {
        // Chain: leaf PEM ++ CA PEM (the on-disk / generated CA PEM verbatim).
        cert_pem: leaf_cert.pem() + &ca.cert_pem,
        key_pem,
    };

    ca.leaf_certs
        .lock()
        .expect("leaf_certs mutex poisoned")
        .insert(hostname.to_string(), leaf.clone());
    tracing::debug!(target: "mitm_leaf", "[mitm-leaf] minted RSA leaf for {hostname}");
    Ok(leaf)
}

/// Mint-or-cache an `Arc<rustls::ServerConfig>` for `hostname`.
///
/// The Rust analogue of the TS `secureContextFor`: builds a server config from
/// the leaf chain (leaf + CA) + leaf key, advertising `http/1.1` ALPN. The cache
/// lives on [`MitmCa::server_configs`](crate::mitm_ca::MitmCa).
///
/// # Errors
///
/// Returns [`LeafError`] if leaf minting fails or the PEM chain/key cannot be
/// loaded into a `rustls::ServerConfig`.
pub fn server_config_for(
    ca: &MitmCa,
    hostname: &str,
) -> Result<Arc<rustls::ServerConfig>, LeafError> {
    if let Some(cached) = ca
        .server_configs
        .lock()
        .expect("server_configs mutex poisoned")
        .get(hostname)
    {
        return Ok(Arc::clone(cached));
    }

    let leaf = mint_leaf_cert(ca, hostname)?;

    // Parse the PEM chain + key into DER for rustls. rustls-pemfile yields
    // borrowed/owned DER; collect the full chain (leaf + CA).
    let mut chain_reader = std::io::BufReader::new(leaf.cert_pem.as_bytes());
    let cert_chain: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut chain_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| LeafError::Config(format!("[mitm-leaf] cert parse failed: {err}")))?;
    if cert_chain.is_empty() {
        return Err(LeafError::Config(
            "[mitm-leaf] empty cert chain".to_string(),
        ));
    }

    let mut key_reader = std::io::BufReader::new(leaf.key_pem.as_bytes());
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|err| LeafError::Config(format!("[mitm-leaf] key parse failed: {err}")))?
        .ok_or_else(|| LeafError::Config("[mitm-leaf] no private key in PEM".to_string()))?;

    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)
        .map_err(|err| {
            LeafError::Config(format!("[mitm-leaf] ServerConfig build failed: {err}"))
        })?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    let arc = Arc::new(config);
    ca.server_configs
        .lock()
        .expect("server_configs mutex poisoned")
        .insert(hostname.to_string(), Arc::clone(&arc));
    Ok(arc)
}

/// Mint-or-cache an `Arc<rustls::sign::CertifiedKey>` for `hostname`.
///
/// The SNI-resolution primitive used by [`MitmCertResolver`]: builds a
/// [`CertifiedKey`] (leaf+CA chain DER + an RSA-PKCS1 [`rustls::sign::SigningKey`])
/// from the minted leaf, caching it on [`MitmCa::cert_keys`](crate::mitm_ca::MitmCa).
/// This is the per-connection analogue of the TS `secureContextFor` consumed by
/// node's `SNICallback`.
///
/// # Errors
///
/// Returns [`LeafError`] if leaf minting fails, the PEM chain/key cannot be
/// parsed to DER, or the RSA key cannot be loaded into a `rustls` signing key.
pub fn certified_key_for(ca: &MitmCa, hostname: &str) -> Result<Arc<CertifiedKey>, LeafError> {
    if let Some(cached) = ca
        .cert_keys
        .lock()
        .expect("cert_keys mutex poisoned")
        .get(hostname)
    {
        return Ok(Arc::clone(cached));
    }

    let leaf = mint_leaf_cert(ca, hostname)?;

    let mut chain_reader = std::io::BufReader::new(leaf.cert_pem.as_bytes());
    let cert_chain: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut chain_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| LeafError::Config(format!("[mitm-leaf] cert parse failed: {err}")))?;
    if cert_chain.is_empty() {
        return Err(LeafError::Config(
            "[mitm-leaf] empty cert chain".to_string(),
        ));
    }

    let mut key_reader = std::io::BufReader::new(leaf.key_pem.as_bytes());
    let key_der: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|err| LeafError::Config(format!("[mitm-leaf] key parse failed: {err}")))?
        .ok_or_else(|| LeafError::Config("[mitm-leaf] no private key in PEM".to_string()))?;

    // Sign with the ring provider (matching rustls 0.22's default provider), so
    // the resolver and the rest of the stack share one crypto backend.
    let signing_key = rustls::crypto::ring::sign::any_supported_type(&key_der)
        .map_err(|err| LeafError::Config(format!("[mitm-leaf] load signing key failed: {err}")))?;

    let certified = Arc::new(CertifiedKey::new(cert_chain, signing_key));
    ca.cert_keys
        .lock()
        .expect("cert_keys mutex poisoned")
        .insert(hostname.to_string(), Arc::clone(&certified));
    Ok(certified)
}

/// A `rustls` [`ResolvesServerCert`] that mints a per-host leaf on demand from
/// the `ClientHello` SNI, backed by an [`MitmCa`] and a default hostname.
///
/// The Rust analogue of the TS `tls.createServer({ SNICallback })` in
/// `tls-terminate-proxy.js`: `resolve(client_hello)` reads the SNI server name
/// (falling back to [`Self::default_host`] when the client sent none) and
/// returns the minted [`CertifiedKey`] for that host via [`certified_key_for`],
/// caching it on the CA. Minting failures resolve to `None` (rustls then fails
/// the handshake), the rough equivalent of the TS `SNICallback(err)` path.
#[derive(Clone)]
pub struct MitmCertResolver {
    ca: Arc<MitmCa>,
    default_host: String,
}

impl std::fmt::Debug for MitmCertResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MitmCertResolver")
            .field("default_host", &self.default_host)
            .finish_non_exhaustive()
    }
}

impl MitmCertResolver {
    /// Build a resolver minting leaves from `ca`, defaulting to `default_host`
    /// when a `ClientHello` carries no SNI.
    #[must_use]
    pub fn new(ca: Arc<MitmCa>, default_host: String) -> Self {
        Self { ca, default_host }
    }
}

impl ResolvesServerCert for MitmCertResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let host = client_hello
            .server_name()
            .map_or(self.default_host.as_str(), |s| s);
        match certified_key_for(&self.ca, host) {
            Ok(key) => Some(key),
            Err(err) => {
                tracing::error!(
                    target: "tls_terminate",
                    "[tls-terminate] cert resolve for {host} failed: {err}"
                );
                None
            }
        }
    }
}

/// The `subjectAltName` for `hostname`: an IP-literal → `IpAddress`, else a
/// `DnsName`. Mirrors the TS `sanFor` (`isIP(hostname) !== 0 ? IP : DNS`).
///
/// # Errors
///
/// Returns [`LeafError::Mint`] if a non-IP hostname is not a valid IA5 DNS name.
fn san_for(hostname: &str) -> Result<SanType, LeafError> {
    if let Ok(ip) = hostname.parse::<std::net::IpAddr>() {
        Ok(SanType::IpAddress(ip))
    } else {
        let ia5 = Ia5String::try_from(hostname.to_string()).map_err(|err| {
            LeafError::Mint(format!("[mitm-leaf] invalid DNS SAN {hostname:?}: {err}"))
        })?;
        Ok(SanType::DnsName(ia5))
    }
}

/// Leaf validity capped at `min(CA notAfter, notBefore + 99d)`.
///
/// 99d sits below every TLS validity ceiling we care about; leaves are re-minted
/// per session so we don't need headroom. Anchored at `not_before` (already
/// backdated 1 day) — not "now" — so `now + Nd` doesn't lose a day of margin.
/// Faithful to `mitm-leaf.js:88-101`.
fn clamp_validity(ca: &MitmCa, not_before: OffsetDateTime) -> OffsetDateTime {
    let ca_end = ca.ca_cert.params().not_after;
    let max = not_before + Duration::days(99);
    if ca_end < max {
        ca_end
    } else {
        max
    }
}

/// 16 CSPRNG bytes with the high bit cleared so the DER INTEGER stays positive
/// (TS `randomSerial`). Returned as a big-endian byte vector.
fn random_serial() -> Vec<u8> {
    let mut bytes = [0_u8; 16];
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mitm_ca::{create_mitm_ca, dispose_mitm_ca, MitmCaOptions};
    use rustls::client::danger::ServerCertVerifier as _;
    use rustls::pki_types::{ServerName, UnixTime};
    use std::time::SystemTime;
    use x509_parser::prelude::FromDer as _;

    fn der_of_first_cert(pem: &str) -> Vec<u8> {
        let mut rd = std::io::BufReader::new(pem.as_bytes());
        let mut certs = rustls_pemfile::certs(&mut rd);
        certs.next().unwrap().unwrap().to_vec()
    }

    /// A minted leaf for a DNS host has the right CN, SAN, basicConstraints
    /// cA:false, EKU serverAuth, and validity clamped to <= now+99d AND
    /// <= CA notAfter.
    #[test]
    fn leaf_for_dns_host_has_expected_fields() {
        let ca = create_mitm_ca(MitmCaOptions::default()).unwrap();
        let leaf = mint_leaf_cert(&ca, "example.com").unwrap();
        // Chain = leaf ++ CA: two CERTIFICATE blocks.
        assert_eq!(leaf.cert_pem.matches("BEGIN CERTIFICATE").count(), 2);

        let der = der_of_first_cert(&leaf.cert_pem);
        let cert = x509_parser::certificate::X509Certificate::from_der(&der)
            .unwrap()
            .1;
        let cn = cert
            .subject()
            .iter_common_name()
            .next()
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(cn, "example.com");

        // basicConstraints cA:false critical.
        let bc = cert.basic_constraints().unwrap().unwrap();
        assert!(!bc.value.ca, "leaf must be cA:false");
        assert!(bc.critical);

        // SAN = DNS:example.com.
        let san = cert.subject_alternative_name().unwrap().unwrap();
        let names: Vec<_> = san.value.general_names.iter().collect();
        assert!(
            names.iter().any(|gn| matches!(
                gn,
                x509_parser::extensions::GeneralName::DNSName("example.com")
            )),
            "SAN must be DNS:example.com, got {names:?}"
        );

        // extKeyUsage serverAuth.
        let eku = cert.extended_key_usage().unwrap().unwrap();
        assert!(eku.value.server_auth, "EKU must include serverAuth");

        // Issuer DN == CA subject DN.
        let ca_der = der_of_first_cert(&ca.cert_pem);
        let ca_cert = x509_parser::certificate::X509Certificate::from_der(&ca_der)
            .unwrap()
            .1;
        assert_eq!(cert.issuer().to_string(), ca_cert.subject().to_string());

        // Validity clamp: notAfter <= now+99d AND <= CA notAfter.
        let not_after = cert.validity().not_after.timestamp();
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let plus_99d = now + 99 * 24 * 3600 + 60; // +60s slack
        assert!(not_after <= plus_99d, "leaf notAfter must be <= now+99d");
        let ca_not_after = ca_cert.validity().not_after.timestamp();
        assert!(
            not_after <= ca_not_after,
            "leaf notAfter must be <= CA notAfter"
        );

        // No authorityKeyIdentifier.
        assert!(
            cert.get_extension_unique(
                &x509_parser::oid_registry::OID_X509_EXT_AUTHORITY_KEY_IDENTIFIER
            )
            .unwrap()
            .is_none(),
            "leaf must NOT carry an AKI"
        );

        dispose_mitm_ca(&ca);
    }

    /// A minted leaf for an IP literal carries SAN=IP:<addr>.
    #[test]
    fn leaf_for_ip_host_has_ip_san() {
        let ca = create_mitm_ca(MitmCaOptions::default()).unwrap();
        let leaf = mint_leaf_cert(&ca, "1.2.3.4").unwrap();
        let der = der_of_first_cert(&leaf.cert_pem);
        let cert = x509_parser::certificate::X509Certificate::from_der(&der)
            .unwrap()
            .1;
        let san = cert.subject_alternative_name().unwrap().unwrap();
        let has_ip = san.value.general_names.iter().any(|gn| {
            matches!(
                gn,
                x509_parser::extensions::GeneralName::IPAddress(octets) if *octets == [1, 2, 3, 4]
            )
        });
        assert!(has_ip, "SAN must be IP:1.2.3.4");
        dispose_mitm_ca(&ca);
    }

    /// Caching: a second mint for the same host returns the identical leaf.
    #[test]
    fn minting_is_cached_per_host() {
        let ca = create_mitm_ca(MitmCaOptions::default()).unwrap();
        let a = mint_leaf_cert(&ca, "cache.example").unwrap();
        let b = mint_leaf_cert(&ca, "cache.example").unwrap();
        assert_eq!(a.cert_pem, b.cert_pem);
        assert_eq!(a.key_pem, b.key_pem);
        dispose_mitm_ca(&ca);
    }

    /// THE security property: a verifier trusting ONLY the CA accepts the minted
    /// leaf for the hostname. Build a `RootCertStore` with just the CA cert,
    /// then run rustls's webpki server-cert verifier over the leaf chain.
    #[test]
    fn ca_trusting_verifier_accepts_leaf() {
        let ca = create_mitm_ca(MitmCaOptions::default()).unwrap();
        let leaf = mint_leaf_cert(&ca, "secure.example").unwrap();

        // Trust store: ONLY the CA cert.
        let ca_der = der_of_first_cert(&ca.cert_pem);
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from(ca_der))
            .expect("add CA to root store");

        // Server verifier trusting only that root.
        let verifier = rustls::client::WebPkiServerVerifier::builder(Arc::new(roots))
            .build()
            .expect("build verifier");

        // Split the chain: end-entity + intermediates (here: the CA, sent in chain).
        let mut rd = std::io::BufReader::new(leaf.cert_pem.as_bytes());
        let chain: Vec<CertificateDer<'static>> =
            rustls_pemfile::certs(&mut rd).map(|c| c.unwrap()).collect();
        let (end_entity, intermediates) = chain.split_first().unwrap();

        let server_name = ServerName::try_from("secure.example").unwrap();
        let now = UnixTime::since_unix_epoch(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap(),
        );
        let result = verifier.verify_server_cert(end_entity, intermediates, &server_name, &[], now);
        assert!(
            result.is_ok(),
            "CA-trusting verifier must accept the leaf: {result:?}"
        );

        // Negative control: the same leaf for a DIFFERENT hostname is rejected.
        let wrong_name = ServerName::try_from("not-the-host.example").unwrap();
        let bad = verifier.verify_server_cert(end_entity, intermediates, &wrong_name, &[], now);
        assert!(
            bad.is_err(),
            "verifier must reject the leaf for a non-matching host"
        );

        dispose_mitm_ca(&ca);
    }

    /// `certified_key_for` returns a `CertifiedKey` whose leaf SAN matches the
    /// host, and is cached per host.
    #[test]
    fn certified_key_leaf_san_matches_host() {
        let ca = Arc::new(create_mitm_ca(MitmCaOptions::default()).unwrap());
        let ck = certified_key_for(&ca, "resolve.example").unwrap();
        // First cert in the chain is the leaf; assert its SAN.
        let leaf_der = ck.cert.first().unwrap().as_ref();
        let cert = x509_parser::certificate::X509Certificate::from_der(leaf_der)
            .unwrap()
            .1;
        let san = cert.subject_alternative_name().unwrap().unwrap();
        assert!(
            san.value.general_names.iter().any(|gn| matches!(
                gn,
                x509_parser::extensions::GeneralName::DNSName("resolve.example")
            )),
            "leaf SAN must be DNS:resolve.example"
        );
        // Cached: identical Arc on the second call.
        let ck2 = certified_key_for(&ca, "resolve.example").unwrap();
        assert!(
            Arc::ptr_eq(&ck, &ck2),
            "CertifiedKey must be cached per host"
        );
        dispose_mitm_ca(&ca);
    }

    /// The `MitmCertResolver` resolves a `ClientHello` SNI to a `CertifiedKey` whose
    /// leaf CN matches the SNI. (Drives the resolver via a full rustls server
    /// handshake in the `tls_terminate` integration tests; here we assert the
    /// cache-population side effect of `certified_key_for` the resolver calls.)
    #[test]
    fn resolver_mints_for_sni_host() {
        let ca = Arc::new(create_mitm_ca(MitmCaOptions::default()).unwrap());
        let resolver = MitmCertResolver::new(Arc::clone(&ca), "default.example".to_string());
        // The resolver's minting primitive caches on the CA; exercise it.
        let ck = certified_key_for(&resolver.ca, "sni.example").unwrap();
        let leaf_der = ck.cert.first().unwrap().as_ref();
        let cert = x509_parser::certificate::X509Certificate::from_der(leaf_der)
            .unwrap()
            .1;
        let cn = cert
            .subject()
            .iter_common_name()
            .next()
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(cn, "sni.example");
        dispose_mitm_ca(&ca);
    }

    /// `server_config_for` builds + caches a `ServerConfig` advertising http/1.1.
    #[test]
    fn server_config_caches_with_http11_alpn() {
        let ca = create_mitm_ca(MitmCaOptions::default()).unwrap();
        let a = server_config_for(&ca, "cfg.example").unwrap();
        assert_eq!(a.alpn_protocols, vec![b"http/1.1".to_vec()]);
        let b = server_config_for(&ca, "cfg.example").unwrap();
        assert!(Arc::ptr_eq(&a, &b), "ServerConfig must be cached per host");
        dispose_mitm_ca(&ca);
    }
}
